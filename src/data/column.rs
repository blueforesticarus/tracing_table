//! Borrowed column codecs. Schemas and decoded coefficients are temporary views;
//! archives retain only selected metadata records and one size byte per field.
use smallvec::SmallVec;

use super::{blockbuffer::ArchivedFrame, dataframe::Frame, util::float_u16::FloatU16};

/// Tuning bounds fitting work and dictionary metadata; neither is a wire capacity.
pub const MAX_DICTIONARY_VALUES: usize = 64;
pub const MAX_CYCLE: u8 = 255;
// Tuning: favor predictable fitting work and simpler encodings over tiny savings.
pub const MAX_CYCLE_CANDIDATES: usize = 4;
pub const MIN_ENCODING_SAVINGS: usize = 8;
const CYCLE_ANCHORS: usize = 8;

/// Supplied by the caller, never copied into each block or its footer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub offset: u16,
    pub width: u8,
    pub strings: bool,
}

impl Layout {
    pub fn new(offset: usize, width: usize, strings: bool) -> Self {
        assert!(matches!(width, 1 | 2 | 4 | 8 | 16));
        assert!(!strings || width == 2);
        Self {
            offset: offset.try_into().expect("column position exceeds u16"),
            width: width as u8,
            strings,
        }
    }
    fn range(self) -> std::ops::Range<usize> {
        let start = usize::from(self.offset);
        start..start + usize::from(self.width)
    }
}

#[inline]
fn word_mask(width: usize) -> u128 {
    u128::MAX >> (128 - width * 8)
}

/// Modular ZigZag is shared by native-width fields and fixed structural columns.
#[inline]
pub(crate) fn residual(value: u128, prediction: u128, width: usize) -> u128 {
    let delta = value.wrapping_sub(prediction) & word_mask(width);
    ((delta << 1) ^ 0u128.wrapping_sub(delta >> (width * 8 - 1))) & word_mask(width)
}
#[inline]
pub(crate) fn restore(encoded: u128, prediction: u128, width: usize) -> u128 {
    prediction.wrapping_add((encoded >> 1) ^ 0u128.wrapping_sub(encoded & 1)) & word_mask(width)
}

/// Preserve the existing timestamp/parent rational rounding, without float math.
#[inline]
pub(crate) fn advance(stride: u64, row: usize, length: usize) -> u64 {
    if stride == 0 {
        return 0;
    }
    assert!(row < length);
    ((u128::from(stride) * row as u128 + length as u128 / 2) / length as u128) as u64
}

/// Endpoint fitting is shared; only parents quantize their origin as well.
pub(crate) fn fit_line(
    first: usize,
    start: u128,
    last: usize,
    end: u128,
    length: usize,
    quantized_origin: bool,
) -> (u128, FloatU16) {
    let quantize = |value: u128| {
        if quantized_origin {
            u128::from(FloatU16::from_u64_lossy(value.min(u64::MAX as u128) as u64).to_u64())
        } else {
            value
        }
    };
    if first == last || end <= start {
        return (quantize(start), FloatU16::default());
    }
    let run = (last - first) as u128;
    let rise = end - start;
    let origin = start.saturating_mul(run);
    let projected = rise.saturating_mul(first as u128);
    let (offset, stride) = if origin < projected {
        (0, end.saturating_mul(length as u128) / last as u128)
    } else {
        (
            quantize((origin - projected) / run),
            rise.saturating_mul(length as u128) / run,
        )
    };
    (
        offset,
        FloatU16::from_u64_lossy(stride.min(u64::MAX as u128) as u64),
    )
}

/// Mutate one physical column and replace only its mask; unrelated edit locks survive.
pub(crate) fn rewrite(
    frame: &mut Frame,
    layout: Layout,
    mut encode: impl FnMut(u128, usize, &[u8]) -> u128,
) {
    let mut first = 0;
    let mut mask = 0;
    for (index, row) in frame.buffer[frame.unit..(frame.length + 1) * frame.unit]
        .chunks_exact_mut(frame.unit)
        .enumerate()
    {
        let value = encode(read_word(&row[layout.range()]), index, row);
        write_word(&mut row[layout.range()], value);
        if index == 0 {
            first = value;
        }
        mask |= value ^ first;
    }
    write_word(&mut frame.buffer[layout.range()], mask);
}

/// Timestamp starts retain unsigned offsets when flat or outside the coefficient
/// range. Fit only their immutable starts, never late close/range-end values.
pub(crate) fn fit_timestamp(frame: &mut Frame) -> FloatU16 {
    if frame.length < 2 {
        return FloatU16::default();
    }
    let end = read_word(&frame.buffer[frame.length * frame.unit..frame.length * frame.unit + 8]);
    let stride = end * frame.length as u128 / (frame.length - 1) as u128;
    if stride > u128::from(FloatU16::MAX) {
        return FloatU16::default();
    }
    let stride = FloatU16::from_u64_lossy(stride as u64);
    if stride.to_u64() != 0 {
        let length = frame.length;
        rewrite(frame, Layout::new(0, 8, false), |value, row, _| {
            residual(value, u128::from(advance(stride.to_u64(), row, length)), 8)
        });
    }
    stride
}

pub(crate) fn read_word(bytes: &[u8]) -> u128 {
    let mut word = [0; 16];
    // Rows use native byte order, matching BytePacker on either endian target.
    #[cfg(target_endian = "little")]
    word[..bytes.len()].copy_from_slice(bytes);
    #[cfg(target_endian = "big")]
    word[16 - bytes.len()..].copy_from_slice(bytes);
    u128::from_ne_bytes(word)
}
pub(crate) fn write_word(bytes: &mut [u8], value: u128) {
    let word = value.to_ne_bytes();
    #[cfg(target_endian = "little")]
    bytes.copy_from_slice(&word[..bytes.len()]);
    #[cfg(target_endian = "big")]
    bytes.copy_from_slice(&word[16 - bytes.len()..]);
}

// Only the selected form is serialized. Row mode is retained for experimental
// string-index behavior; general fields use the two-byte custom-float stride.
const OFFSET: u8 = 1;
const LINEAR: u8 = 2;
const ROW: u8 = 3;
const CYCLE: u8 = 4;
const ROW_CYCLE: u8 = 5;
const DICTIONARY: u8 = 6;
const DICTIONARY_CYCLE: u8 = 7;
// Predict slowly advancing dictionary keys without wrapping at a cycle boundary.
const DICTIONARY_STRIDE: u8 = 8;

#[derive(Clone, Copy, Debug, Default)]
struct Prediction {
    origin: u128,
    stride: u64,
    period: u8,
    phase: u8,
    row: bool,
    descending: bool,
}

impl Prediction {
    fn at(self, row: usize, length: usize) -> u128 {
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
            row as u128
        } else {
            advance(self.stride, row, length) as u128
        };
        if self.descending {
            self.origin.wrapping_sub(increase)
        } else {
            self.origin.wrapping_add(increase)
        }
    }
    fn record(self, width: usize) -> SmallVec<[u8; 24]> {
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
    fn parse(bytes: &[u8], width: usize) -> Self {
        let tag = bytes[0] & 0x7f;
        assert!(
            matches!(tag, OFFSET | LINEAR | ROW | CYCLE | ROW_CYCLE),
            "unknown column prediction"
        );
        let mut origin = [0; 16];
        origin[..width].copy_from_slice(&bytes[1..1 + width]);
        let mut prediction = Self {
            origin: u128::from_le_bytes(origin),
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
        assert_eq!(bytes.len(), end, "column prediction size mismatch");
        prediction
    }
}

/// The directory is schema-sized. Prefix sums locate records, so the combined
/// metadata region has no 255-byte limit; individual records have that bound.
#[derive(Clone, Copy)]
pub struct Metadata<'a, 's> {
    records: &'a [u8],
    widths: &'a [u8],
    layout: &'s [Layout],
}

impl<'a, 's> Metadata<'a, 's> {
    pub fn split(trailer: &'a [u8], layout: &'s [Layout]) -> (&'a [u8], Self) {
        let directory = trailer
            .len()
            .checked_sub(layout.len())
            .expect("column directory exceeds footer");
        let widths = &trailer[directory..];
        let size: usize = widths.iter().map(|&n| usize::from(n)).sum();
        let start = directory
            .checked_sub(size)
            .expect("column metadata exceeds footer");
        (
            &trailer[..start],
            Self {
                records: &trailer[start..directory],
                widths,
                layout,
            },
        )
    }
    pub fn storage_bytes(self) -> usize {
        self.records.len() + self.widths.len()
    }
    pub fn has_encodings(self) -> bool {
        !self.records.is_empty()
    }
    pub fn entries(self) -> impl Iterator<Item = (Layout, &'a [u8])> {
        let mut position = 0;
        self.layout
            .iter()
            .copied()
            .zip(self.widths.iter().copied())
            .map(move |(layout, width)| {
                let start = position;
                position += usize::from(width);
                (layout, &self.records[start..position])
            })
    }
    pub fn is_encoded(self, offset: usize) -> bool {
        self.entries()
            .any(|(layout, record)| usize::from(layout.offset) == offset && !record.is_empty())
    }
    /// Exceptional edits drop only dictionaries. Other fitted coefficients and
    /// the parent prefix remain byte-for-byte unchanged, without refitting.
    pub(crate) fn without_dictionaries(self, prefix: &[u8]) -> SmallVec<[u8; 128]> {
        let mut trailer = SmallVec::from_slice(prefix);
        let mut widths = SmallVec::<[u8; 16]>::new();
        for (_, record) in self.entries() {
            let keep = !matches!(
                record.first(),
                Some(&DICTIONARY) | Some(&DICTIONARY_CYCLE) | Some(&DICTIONARY_STRIDE)
            );
            widths.push(if keep { record.len() as u8 } else { 0 });
            if keep {
                trailer.extend_from_slice(record);
            }
        }
        trailer.extend_from_slice(&widths);
        trailer
    }
    pub fn decode(self, frame: &ArchivedFrame, row: &mut [u8], index: usize) {
        for (layout, record) in self.entries().filter(|(_, record)| !record.is_empty()) {
            let column = Column::new(frame, layout, record);
            column.decode_row(row, index);
        }
    }
    pub fn encode(self, frame: &ArchivedFrame, row: &mut [u8], index: usize) -> bool {
        for (layout, record) in self.entries().filter(|(_, record)| !record.is_empty()) {
            let column = Column::new(frame, layout, record);
            let Some(value) = column.encode(read_word(&row[layout.range()]), index) else {
                return false;
            };
            write_word(&mut row[layout.range()], value);
        }
        true
    }
}

/// Temporary view: no schema references, native pointers, or expanded predictors
/// are stored in the archive. Dictionary bases borrow the physical first slot.
pub struct Column<'a> {
    frame: &'a ArchivedFrame,
    layout: Layout,
    codec: Codec<'a>,
}

enum Codec<'a> {
    Raw,
    Predicted(Prediction),
    Dictionary {
        entries: &'a [u8],
        count: usize,
        bits: usize,
        prediction: KeyPrediction,
    },
}

// Key prediction always starts at zero, preserving the first slot's dictionary base.
#[derive(Clone, Copy)]
enum KeyPrediction {
    Raw,
    Cycle(u8, u8),
    Stride(u64),
}
impl KeyPrediction {
    fn at(self, row: usize, length: usize) -> u128 {
        match self {
            Self::Raw => 0,
            Self::Cycle(period, phase) => key_prediction(row, period, phase),
            Self::Stride(stride) => u128::from(advance(stride, row, length)),
        }
    }
    fn encode(self, id: u128, row: usize, length: usize, width: usize) -> u128 {
        if matches!(self, Self::Raw) {
            id
        } else {
            residual(id, self.at(row, length), width)
        }
    }
    fn decode(self, encoded: u128, row: usize, length: usize, width: usize) -> u128 {
        if matches!(self, Self::Raw) {
            encoded
        } else {
            restore(encoded, self.at(row, length), width)
        }
    }
}

impl<'a> Column<'a> {
    pub fn new(frame: &'a ArchivedFrame, layout: Layout, record: &'a [u8]) -> Self {
        assert!(
            layout.range().end <= frame.unit(),
            "column exceeds frame row"
        );
        let codec = match record.first().copied() {
            None => Codec::Raw,
            Some(DICTIONARY | DICTIONARY_CYCLE | DICTIONARY_STRIDE) => {
                let count = usize::from(record[1]);
                let bits = usize::from(record[2]);
                assert!((2..=MAX_DICTIONARY_VALUES).contains(&count));
                assert!((1..=usize::from(layout.width) * 8).contains(&bits));
                let (prediction, header) = if record[0] == DICTIONARY_CYCLE {
                    assert!(
                        record[3] != 0 && record[4] < record[3],
                        "invalid dictionary cycle"
                    );
                    (KeyPrediction::Cycle(record[3], record[4]), 5)
                } else if record[0] == DICTIONARY_STRIDE {
                    let stride =
                        FloatU16::from_bits(u16::from_le_bytes(record[3..5].try_into().unwrap()));
                    (KeyPrediction::Stride(stride.to_u64()), 5)
                } else {
                    (KeyPrediction::Raw, 3)
                };
                assert_eq!(
                    record.len(),
                    header + (count * bits).div_ceil(8),
                    "dictionary size mismatch"
                );
                Codec::Dictionary {
                    entries: &record[header..],
                    count,
                    bits,
                    prediction,
                }
            }
            _ => Codec::Predicted(Prediction::parse(record, usize::from(layout.width))),
        };
        Self {
            frame,
            layout,
            codec,
        }
    }
    pub fn is_dictionary(&self) -> bool {
        matches!(self.codec, Codec::Dictionary { .. })
    }
    pub(crate) fn decode_row(&self, row: &mut [u8], index: usize) {
        let value = self.decode(read_word(&row[self.layout.range()]), index);
        write_word(&mut row[self.layout.range()], value);
    }
    /// Selective random access borrows packed rows and needs at most one word of scratch.
    pub fn read(&self, index: usize) -> u128 {
        let mut bytes = [0; 16];
        let width = usize::from(self.layout.width);
        self.frame
            .read_column(index, usize::from(self.layout.offset), &mut bytes[..width]);
        self.decode(read_word(&bytes[..width]), index)
    }
    pub fn decode(&self, encoded: u128, index: usize) -> u128 {
        let width = usize::from(self.layout.width);
        match self.codec {
            Codec::Raw => encoded,
            Codec::Predicted(prediction) => {
                restore(encoded, prediction.at(index, self.frame.len()), width)
            }
            Codec::Dictionary {
                entries,
                count,
                bits,
                prediction,
            } => {
                let base = read_word(&self.frame.first()[self.layout.range()]);
                let id = prediction.decode(encoded ^ base, index, self.frame.len(), width) as usize;
                assert!(id < count, "dictionary key exceeds entry count");
                base.checked_add(unpack(entries, id, bits))
                    .expect("dictionary value exceeds u128")
            }
        }
    }
    pub fn encode(&self, value: u128, index: usize) -> Option<u128> {
        let width = usize::from(self.layout.width);
        Some(match self.codec {
            Codec::Raw => value,
            Codec::Predicted(prediction) => {
                residual(value, prediction.at(index, self.frame.len()), width)
            }
            Codec::Dictionary {
                entries,
                count,
                bits,
                prediction,
            } => {
                let base = read_word(&self.frame.first()[self.layout.range()]);
                let delta = value.checked_sub(base)?;
                let id = (0..count).find(|&id| unpack(entries, id, bits) == delta)?;
                // Row zero owns the immutable base, rather than a mutable key.
                if index == 0 && id != 0 {
                    return None;
                }
                base ^ prediction.encode(id as u128, index, self.frame.len(), width)
            }
        })
    }
}

fn key_prediction(row: usize, period: u8, phase: u8) -> u128 {
    (((row % usize::from(period) + usize::from(phase)) % usize::from(period)) as u128)
        .wrapping_sub(u128::from(phase))
}
fn unpack(entries: &[u8], id: usize, bits: usize) -> u128 {
    // Consume byte fragments, including the seventeenth byte of an unaligned
    // 128-bit entry, without overreading the record or shifting by 128.
    let mut value = 0;
    let mut done = 0;
    while done < bits {
        let position = id * bits + done;
        let count = (8 - position % 8).min(bits - done);
        value |= u128::from((entries[position / 8] >> (position % 8)) & (u8::MAX >> (8 - count)))
            << done;
        done += count;
    }
    value
}

fn varying_bits(value: u128, width: usize) -> usize {
    if cfg!(feature = "bit-compression") {
        value.count_ones() as usize
    } else {
        value.to_le_bytes()[..width]
            .iter()
            .filter(|&&v| v != 0)
            .count()
            * 8
    }
}
fn frame_bits(frame: &Frame) -> usize {
    frame
        .mask()
        .iter()
        .map(|&byte| varying_bits(u128::from(byte), 1))
        .sum()
}
fn payload_size(frame: &Frame, bits: usize) -> usize {
    let stride = if cfg!(feature = "bit-compression") && super::dataframe::CONTINUOUS_BIT_PACKING {
        bits
    } else {
        bits.div_ceil(8) * 8
    };
    2 * frame.unit + ((frame.length - 1) * stride).div_ceil(8)
}

/// Frame packing is the baseline: it omits any constant bit, not just leading zeros.
struct Cost<'a> {
    frame: &'a Frame,
    total_bits: usize,
    original_bits: usize,
    footer: usize,
    width: usize,
}
impl Cost<'_> {
    fn size(&self, bits: usize, metadata: usize) -> usize {
        (payload_size(self.frame, self.total_bits - self.original_bits + bits)
            + self.footer
            + metadata)
            .div_ceil(2)
            * 2
    }
}

/// Dictionary economics are deliberately explicit. First/mask slots and the
/// one-byte directory entry are shared; the minimum reuses the first slot.
fn dictionary_cost(unique: &[u128], cost: &Cost<'_>) -> (usize, usize) {
    let minimum = *unique.iter().min().unwrap();
    let entry_mask = unique
        .iter()
        .fold(0, |mask, &value| mask | (value - minimum));
    let entry_bits = (u128::BITS - entry_mask.leading_zeros()) as usize;
    let key_bits = (usize::BITS - (unique.len() - 1).leading_zeros()) as usize;
    let key_bits = if cfg!(feature = "bit-compression") {
        key_bits
    } else {
        key_bits.div_ceil(8) * 8
    };
    let metadata_bytes = 3 + (unique.len() * entry_bits).div_ceil(8);
    // Every discovery-order key in 0..K occurs, so the direct mask is exact.
    // Predicted keys require inspecting their residuals separately.
    (cost.size(key_bits, metadata_bytes), entry_bits)
}

struct Candidate {
    record: SmallVec<[u8; 24]>,
    encoded: SmallVec<[u128; 256]>,
    mask: u128,
    size: usize,
}
impl Candidate {
    // Later stages are more complex. Require eight bytes or 1/64 of the current
    // allocation, whichever is larger; near ties retain the earlier simple form.
    fn worthwhile(&self, size: usize) -> bool {
        size.saturating_add(MIN_ENCODING_SAVINGS.max(self.size / 64)) <= self.size
    }

    /// Only shortlisted predictors reach this pass. Residual masks cannot be
    /// inferred from range alone because compaction omits arbitrary constant bits.
    fn consider(
        &mut self,
        record: &[u8],
        values: &[u128],
        cost: &Cost<'_>,
        encode: impl Fn(u128, usize) -> u128,
    ) {
        if record.len() > u8::MAX as usize || !self.worthwhile(cost.size(0, record.len())) {
            return;
        }
        let first = encode(values[0], 0);
        let mut mask = 0;
        for (row, &value) in values.iter().enumerate().skip(1) {
            let previous = mask;
            mask |= encode(value, row) ^ first;
            if mask != previous
                && !self.worthwhile(cost.size(varying_bits(mask, cost.width), record.len()))
            {
                return;
            }
        }
        self.size = cost.size(varying_bits(mask, cost.width), record.len());
        self.mask = mask;
        self.record = SmallVec::from_slice(record);
        self.encoded.clear();
        self.encoded.extend(
            values
                .iter()
                .enumerate()
                .map(|(row, &value)| encode(value, row)),
        );
    }
}

/// Fit either a line or a line evaluated at cyclic row positions. The exact
/// origin compensates for FloatU16 quantization; residuals retain every value bit.
fn prediction(values: &[u128], layout: Layout, period: u8, phase: u8) -> Prediction {
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
        origin: values[0],
        period,
        phase,
        row: layout.strings,
        ..Prediction::default()
    };
    if layout.strings {
        p.origin = p.origin.wrapping_sub(u128::from(phase));
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
    let mask = word_mask(usize::from(layout.width));
    let delta = values[high].wrapping_sub(values[low]) & mask;
    p.descending = delta >> (usize::from(layout.width) * 8 - 1) != 0;
    let rise = if p.descending {
        values[low].wrapping_sub(values[high]) & mask
    } else {
        delta
    };
    p.stride = fit_line(0, 0, run, rise, length, false).1.to_u64();
    let increase = u128::from(advance(p.stride, position(low), length));
    p.origin = if p.descending {
        values[low].wrapping_add(increase)
    } else {
        values[low].wrapping_sub(increase)
    } & mask;
    // A flat fit needs neither cyclic row arithmetic nor stored stride/phase bytes.
    if p.stride == 0 {
        p.period = 0;
        p.phase = 0;
        p.descending = false;
    }
    p
}

/// Discover likely periods from repeated anchor values, rather than trying every
/// period. Eight anchors bound comparisons; vote counts rank at most four fits.
/// Exact and approximate codecs remain lossless even if this heuristic misses a cycle.
fn cycle_periods(values: &[u128]) -> SmallVec<[u8; MAX_CYCLE_CANDIDATES]> {
    let mut anchors = SmallVec::<[(u128, usize); CYCLE_ANCHORS]>::new();
    let mut votes = [0usize; 256];
    for (row, &value) in values.iter().enumerate() {
        if let Some((_, previous)) = anchors.iter_mut().find(|(anchor, _)| *anchor == value) {
            let distance = row - *previous;
            *previous = row;
            // Adjacent equal values are runs, not useful cyclic predictions.
            if (2..=usize::from(MAX_CYCLE)).contains(&distance) {
                votes[distance] += 1;
            }
        } else if anchors.len() < CYCLE_ANCHORS {
            anchors.push((value, row));
        }
    }
    let mut periods = SmallVec::new();
    for _ in 0..MAX_CYCLE_CANDIDATES {
        // Prefer a shorter period when the evidence ties.
        let Some(period) = (2..=usize::from(MAX_CYCLE))
            .filter(|&period| votes[period] != 0)
            .max_by_key(|&period| (votes[period], std::cmp::Reverse(period)))
        else {
            break;
        };
        periods.push(period as u8);
        votes[period] = 0;
    }
    periods
}

/// Cheap forms come first. Bounds eliminate whole families before discovery or
/// residual evaluation; cycle fitting stops at the first worthwhile shortlist hit.
fn select(values: &[u128], layout: Layout, frame: &Frame, footer_bytes: usize) -> Candidate {
    let width = usize::from(layout.width);
    let original_mask = read_word(&frame.mask()[layout.range()]);
    let cost = Cost {
        frame,
        total_bits: frame_bits(frame),
        original_bits: varying_bits(original_mask, width),
        footer: footer_bytes,
        width,
    };
    let mut best = Candidate {
        record: SmallVec::new(),
        encoded: SmallVec::new(),
        mask: original_mask,
        size: cost.size(cost.original_bits, 0),
    };
    if values.len() < 2 || cost.original_bits == 0 {
        return best;
    }
    let origin = Prediction {
        origin: values[0],
        ..Prediction::default()
    };
    consider_prediction(&mut best, origin, values, &cost);
    let line = prediction(values, layout, 0, 0);
    // A flat endpoint fit collapses to the exact origin already considered.
    if line.row || line.stride != 0 {
        consider_prediction(&mut best, line, values, &cost);
    }
    // A compact arithmetic cycle is simpler than an identity dictionary.
    select_cycles(&mut best, values, layout, &cost);
    select_dictionary(&mut best, values, &cost);
    best
}

fn consider_prediction(
    best: &mut Candidate,
    prediction: Prediction,
    values: &[u128],
    cost: &Cost<'_>,
) {
    best.consider(
        &prediction.record(cost.width),
        values,
        cost,
        |value, row| residual(value, prediction.at(row, values.len()), cost.width),
    );
}

fn select_cycles(best: &mut Candidate, values: &[u128], layout: Layout, cost: &Cost<'_>) {
    let metadata_bytes = 1 + cost.width + if layout.strings { 2 } else { 4 };
    if !best.worthwhile(cost.size(0, metadata_bytes)) {
        return;
    }
    let periods = cycle_periods(values);
    if periods.is_empty() {
        return;
    }
    // Locate rotation anchors only when a plausible cycle can afford its footer.
    let distance = |value: u128| {
        let delta = value.wrapping_sub(values[0]) & word_mask(cost.width);
        (delta << (128 - cost.width * 8)) as i128
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
        let phases = if layout.strings {
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

fn select_dictionary(best: &mut Candidate, values: &[u128], cost: &Cost<'_>) {
    // The smallest dictionary has a tag, count, width and at least one entry byte.
    if !best.worthwhile(cost.size(0, 4)) {
        return;
    }
    let width = cost.width;
    let mut unique = SmallVec::<[u128; 64]>::new();
    let mut ids = SmallVec::<[u128; 256]>::new();
    for &value in values {
        let id = match unique.iter().position(|&entry| entry == value) {
            Some(id) => id,
            None if unique.len() < MAX_DICTIONARY_VALUES => {
                unique.push(value);
                unique.len() - 1
            }
            _ => return,
        };
        ids.push(id as u128);
    }
    if unique.len() < 2 {
        return;
    }
    let (direct_cost, entry_bits) = dictionary_cost(&unique, cost);
    let record_bytes = 3 + (unique.len() * entry_bits).div_ceil(8);
    // Even perfect key prediction must pay for the packed entries and cycle bytes.
    if !best.worthwhile(direct_cost) && !best.worthwhile(cost.size(0, record_bytes + 2)) {
        return;
    }
    if record_bytes > u8::MAX as usize {
        return;
    }
    let base = *unique.iter().min().unwrap();
    let mut dictionary = SmallVec::<[u8; 24]>::new();
    dictionary.extend_from_slice(&[DICTIONARY, unique.len() as u8, entry_bits as u8]);
    dictionary.resize(record_bytes, 0);
    for (id, &value) in unique.iter().enumerate() {
        for bit in 0..entry_bits {
            let position = id * entry_bits + bit;
            dictionary[3 + position / 8] |= ((((value - base) >> bit) & 1) as u8) << (position % 8);
        }
    }
    if best.worthwhile(direct_cost) {
        // Discovery assigns every key in 0..K. Their union mask and packed size
        // are known exactly, so direct dictionaries need no residual trial scan.
        let key_bits = (usize::BITS - (unique.len() - 1).leading_zeros()) as usize;
        best.size = direct_cost;
        best.mask = (1u128 << key_bits) - 1;
        best.record = dictionary.clone();
        best.encoded.clear();
        best.encoded.extend(ids.iter().map(|id| base ^ id));
    }
    // Stride and cycle keys have the same minimum metadata cost.
    if !best.worthwhile(cost.size(0, record_bytes + 2)) {
        return;
    }
    // Repeated runs of progressively discovered values benefit from a fractional
    // key stride. Charge its two-byte coefficient before considering cyclic keys.
    let mut predicted = dictionary.clone();
    predicted[0] = DICTIONARY_STRIDE;
    predicted.insert(3, 0);
    predicted.insert(4, 0);
    // Use one fitted key line, ending at the last occurrence of the highest
    // discovery key. A final revisit to an older key must not flatten the line.
    let endpoint = ids.iter().enumerate().max_by_key(|(_, id)| *id).unwrap().0;
    let stride =
        FloatU16::from_u64_lossy((ids[endpoint] * ids.len() as u128 / endpoint as u128) as u64);
    if stride.to_u64() != 0 {
        predicted[3..5].copy_from_slice(&stride.to_bits().to_le_bytes());
        let prediction = KeyPrediction::Stride(stride.to_u64());
        best.consider(&predicted, &ids, cost, |id, row| {
            base ^ prediction.encode(id, row, ids.len(), width)
        });
    }
    // IDs start at zero; cyclic prediction therefore needs no stored origin.
    let first_return = ids
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(row, &id)| (id == 0).then_some(row));
    dictionary[0] = DICTIONARY_CYCLE;
    dictionary.insert(3, 0);
    dictionary.insert(4, 0);
    if !best.worthwhile(cost.size(0, dictionary.len())) {
        return;
    }
    for period in cycle_periods(&ids) {
        let rotated = first_return.map_or(0, |row| {
            ((usize::from(period) - row % usize::from(period)) % usize::from(period)) as u8
        });
        for (choice, phase) in [0, rotated].into_iter().enumerate() {
            if choice == 1 && phase == 0 {
                continue;
            }
            dictionary[3] = period;
            dictionary[4] = phase;
            let previous = best.size;
            best.consider(&dictionary, &ids, cost, |id, row| {
                base ^ KeyPrediction::Cycle(period, phase).encode(id, row, ids.len(), width)
            });
            if best.size < previous {
                return;
            }
        }
    }
}

/// Fit once at rollover. Greedy choices charge real frame packing, the shared
/// directory, record sizes, and two-byte allocation rounding. Ties keep raw data.
pub(crate) fn fit(frame: &mut Frame, trailer: &mut Vec<u8>, layout: &[Layout]) {
    if frame.length == 0 || layout.is_empty() {
        return;
    }
    let mut records = SmallVec::<[u8; 128]>::new();
    let mut sizes = SmallVec::<[u8; 16]>::new();
    let mut values = SmallVec::<[u128; 256]>::new();
    let mut end = 0;
    for &column in layout {
        assert!(
            column.range().start >= end && column.range().end <= frame.unit,
            "invalid column layout"
        );
        end = column.range().end;
        // Constant columns already occupy no packed row bits and need no fitting scratch.
        if read_word(&frame.mask()[column.range()]) == 0 {
            sizes.push(0);
            continue;
        }
        values.clear();
        values.extend(frame.iter().map(|row| read_word(&row[column.range()])));
        let selected = select(
            &values,
            column,
            frame,
            trailer.len() + records.len() + layout.len(),
        );
        let bits = frame_bits(frame)
            - varying_bits(
                read_word(&frame.mask()[column.range()]),
                usize::from(column.width),
            )
            + varying_bits(selected.mask, usize::from(column.width));
        let fits = payload_size(frame, bits)
            + trailer.len()
            + records.len()
            + selected.record.len()
            + layout.len()
            <= u16::MAX as usize;
        if !selected.record.is_empty() && fits {
            rewrite(frame, column, |_, row, _| selected.encoded[row]);
            sizes.push(selected.record.len() as u8);
            records.extend_from_slice(&selected.record);
        } else {
            sizes.push(0);
        }
    }
    trailer.extend_from_slice(&records);
    trailer.extend_from_slice(&sizes);
}

#[cfg(test)]
mod tests;

// Inspection is compiled out by default and never instruments the recording path.
#[cfg(feature = "column-metrics")]
pub mod metrics;
