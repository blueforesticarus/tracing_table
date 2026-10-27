//! Borrowed column storage. Schemas and decoded coefficients are temporary views;
//! archives retain only selected metadata records and one size byte per field.
use super::encoding::common::*;
use super::encoding::dictionary::select_dictionary;
use super::{blockbuffer::ArchivedFrame, dataframe::Frame};
use smallvec::SmallVec;
// Preserve existing column imports while encoding owns every codec implementation.
pub use super::encoding::{
    AnyEncoding, DictionaryEncoding, Encoding, ExceptionsEncoding, Footer, MAX_CYCLE,
    MAX_CYCLE_CANDIDATES, MAX_DICTIONARY_VALUES, MAX_RECORD_BYTES, MIN_ENCODING_SAVINGS,
    OriginEncoding, PredictionEncoding, RawEncoding, UNSIGNED_MASK_CANDIDATES,
    UNSIGNED_MASK_STRIDE, UNSIGNED_MINIMAX_STRIDE, dictionary, exceptions, prediction, raw,
    reference,
    SplitDictionaryEncoding, split_dictionary,
};

pub mod physical;

/// Policy occupies the former string boolean byte; schemas retain their size.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Values,
    Strings,
    Unsigned,
    TaggedOrigin,
}

/// Supplied by the caller, never copied into each block or its footer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub offset: u16,
    pub width: u8,
    pub policy: Policy,
}

impl Layout {
    pub fn new(offset: usize, width: usize, strings: bool) -> Self {
        assert!(matches!(width, 1 | 2 | 4 | 8 | 16));
        assert!(!strings || width == 2);
        Self {
            offset: offset.try_into().expect("column position exceeds u16"),
            width: width as u8,
            policy: if strings {
                Policy::Strings
            } else {
                Policy::Values
            },
        }
    }
    /// Select the fixed-u64 origin predictor at a caller-supplied field position.
    pub fn unsigned(offset: usize, width: usize) -> Self {
        assert_eq!(width, 8, "unsigned-origin fitting requires u64 words");
        Self {
            policy: Policy::Unsigned,
            ..Self::new(offset, width, false)
        }
    }
    pub fn is_strings(self) -> bool {
        self.policy == Policy::Strings
    }
    pub fn is_unsigned(self) -> bool {
        matches!(self.policy, Policy::Unsigned | Policy::TaggedOrigin)
    }
    /// Caller chooses a 60-bit origin with a packed representation tag; ordinary
    /// unsigned columns retain their full-width coefficients and wire format.
    pub fn tagged_origin(offset: usize) -> Self {
        Self {
            policy: Policy::TaggedOrigin,
            ..Self::unsigned(offset, 8)
        }
    }
    pub fn is_tagged_origin(self) -> bool {
        self.policy == Policy::TaggedOrigin
    }
    pub fn range(self) -> std::ops::Range<usize> {
        let start = usize::from(self.offset);
        start..start + usize::from(self.width)
    }
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

#[inline]
pub(crate) fn read_word(bytes: &[u8]) -> u128 {
    let mut word = [0; 16];
    // Rows use native byte order, matching BytePacker on either endian target.
    #[cfg(target_endian = "little")]
    word[..bytes.len()].copy_from_slice(bytes);
    #[cfg(target_endian = "big")]
    word[16 - bytes.len()..].copy_from_slice(bytes);
    u128::from_ne_bytes(word)
}
#[inline]
pub(crate) fn write_word(bytes: &mut [u8], value: u128) {
    let word = value.to_ne_bytes();
    #[cfg(target_endian = "little")]
    bytes.copy_from_slice(&word[..bytes.len()]);
    #[cfg(target_endian = "big")]
    bytes.copy_from_slice(&word[16 - bytes.len()..]);
}

/// The directory is schema-sized. Sizes count backward from the directory start,
/// with entry zero adjacent to it; the combined metadata has no per-record limit.
/// Each directory byte counts two-byte units, allowing up to 510 bytes per record
/// including even-length padding.
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
        let size: usize = widths.iter().map(|&n| usize::from(n) << 1).sum();
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
    /// Immutable codecs can be excluded from a physical payload edit. Their
    /// presence alone must not turn an otherwise direct write into a scratch copy.
    pub fn has_encodings_selected(self, unsigned: bool) -> bool {
        if unsigned {
            return self.has_encodings();
        }
        let skipped: usize = self
            .layout
            .iter()
            .zip(self.widths.iter())
            .filter(|(layout, _)| layout.is_unsigned())
            .map(|(_, width)| usize::from(*width) * 2)
            .sum();
        self.records.len() > skipped
    }
    pub fn entries(self) -> impl Iterator<Item = (Layout, &'a [u8])> {
        let mut position = self.records.len();
        self.layout
            .iter()
            .copied()
            .zip(self.widths.iter().copied())
            .map(move |(layout, width)| {
                let end = position;
                position -= usize::from(width) << 1;
                (layout, &self.records[position..end])
            })
    }
    pub fn is_encoded(self, offset: usize) -> bool {
        self.entries()
            .any(|(layout, record)| usize::from(layout.offset) == offset && !record.is_empty())
    }
    pub fn decode(self, frame: &ArchivedFrame, row: &mut [u8], index: usize) {
        self.decode_selected(frame, row, index, true);
    }
    pub fn decode_selected(
        self,
        frame: &ArchivedFrame,
        row: &mut [u8],
        index: usize,
        unsigned: bool,
    ) {
        for (layout, record) in self
            .entries()
            .filter(|(c, record)| !record.is_empty() && (unsigned || !c.is_unsigned()))
        {
            let column = Column::new(frame, layout, record);
            column.decode_row(row, index);
        }
    }
    pub fn encode(self, frame: &ArchivedFrame, row: &mut [u8], index: usize) -> bool {
        self.encode_selected(frame, row, index, true)
    }
    pub fn encode_selected(
        self,
        frame: &ArchivedFrame,
        row: &mut [u8],
        index: usize,
        unsigned: bool,
    ) -> bool {
        for (layout, record) in self
            .entries()
            .filter(|(c, record)| !record.is_empty() && (unsigned || !c.is_unsigned()))
        {
            let column = Column::new(frame, layout, record);
            let Some(value) = column.encode(read_word(&row[layout.range()]), index) else {
                return false;
            };
            write_word(&mut row[layout.range()], value);
        }
        true
    }
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
    prediction::fit_candidates(&mut best, values, layout, &cost);
    select_dictionary(&mut best, values, &cost);
    if !layout.is_strings() {
        // Zero policy is supplied in code, never in a column descriptor/record.
        // Keeping zero special also preserves duration open sentinels exactly.
        ExceptionsEncoding::<true>::fit(&mut best, values, &cost);
    }
    best
}

/// Fit decoded columns at rollover or after an archived edit requires rebuilding.
/// Greedy choices charge real frame packing, the shared
/// directory, record sizes, and two-byte allocation rounding. Ties keep raw data.
pub(crate) fn fit(frame: &mut Frame, trailer: &mut Vec<u8>, layout: &[Layout]) {
    fit_inner(frame, trailer, layout, None, None);
}
pub(crate) fn fit_relative(
    frame: &mut Frame,
    trailer: &mut Vec<u8>,
    layout: &[Layout],
    origin: u64,
) {
    fit_inner(frame, trailer, layout, None, Some(origin));
}
pub(crate) fn refit(
    frame: &mut Frame,
    trailer: &mut Vec<u8>,
    layout: &[Layout],
    preserved: Metadata<'_, '_>,
) {
    fit_inner(frame, trailer, layout, Some(preserved), None);
}
fn fit_inner(
    frame: &mut Frame,
    trailer: &mut Vec<u8>,
    layout: &[Layout],
    preserved: Option<Metadata<'_, '_>>,
    relative_origin: Option<u64>,
) {
    if frame.length == 0 || layout.is_empty() {
        return;
    }
    let mut records = SmallVec::<[u8; 128]>::new();
    let mut sizes = SmallVec::<[u8; 16]>::new();
    let mut values = SmallVec::<[u128; 256]>::new();
    // Fit mandatory origin fields first so ordinary candidates see exactly the
    // established residual masks, regardless of their directory position.
    let mut fixed = SmallVec::<[Footer; 2]>::new();
    for &column in layout.iter().filter(|c| c.is_unsigned()) {
        assert!(column.range().end <= frame.unit, "invalid column layout");
        let mut footer = Footer::new();
        if let Some(old) = preserved {
            let (_, record) = old
                .entries()
                .find(|(c, _)| c.offset == column.offset)
                .unwrap();
            footer.extend_from_slice(record);
        } else if column.is_tagged_origin() {
            // Preserve decoded input before the legacy absolute predictor
            // rewrites it. The split search must see the same raw/legacy/local
            // alternatives as the offline probe, not detrend residuals twice.
            let original: SmallVec<[u128; 256]> = frame
                .iter()
                .map(|row| read_word(&row[column.range()]))
                .collect();
            // Relative values are already elapsed ticks. Retain an absolute
            // block start in their tagged word without transforming rows again.
            let encoding = match relative_origin {
                Some(origin) => OriginEncoding::relative(origin),
                None => OriginEncoding::absolute(prediction::fit_unsigned(frame, column)),
            };
            encoding.write_footer(8, &mut footer);
            let cost = Cost {
                frame,
                total_bits: frame_bits(frame),
                original_bits: varying_bits(read_word(&frame.mask()[column.range()]), 8),
                footer: trailer.len()
                    + layout.len()
                    + fixed.iter().map(|r| record_storage_bytes(r.len())).sum::<usize>(),
                width: 8,
            };
            let mut candidate = Candidate {
                record: footer.clone(),
                encoded: SmallVec::new(),
                mask: read_word(&frame.mask()[column.range()]),
                size: cost.size(cost.original_bits, footer.len()),
            };
            let scope = if encoding.is_relative() {
                split_dictionary::Scope::Relative(encoding.origin())
            } else {
                split_dictionary::Scope::Absolute(encoding.origin())
            };
            SplitDictionaryEncoding::fit(
                &mut candidate,
                &original,
                &cost,
                scope,
                if encoding.is_relative() { 1 } else { encoding.stride_float().to_u64() },
            );
            if !candidate.encoded.is_empty() {
                rewrite(frame, column, |_, row, _| candidate.encoded[row]);
                footer = candidate.record;
            }
        } else {
            assert!(
                relative_origin.is_none(),
                "relative origin requires tagged schema"
            );
            prediction::fit_unsigned(frame, column)
                .write_footer(usize::from(column.width), &mut footer);
        }
        fixed.push(footer);
    }
    let mut pending: usize = fixed
        .iter()
        .map(|record| record_storage_bytes(record.len()))
        .sum();
    let fixed_count = fixed.len();
    let mut fixed_index = 0;
    let mut end = 0;
    for &column in layout {
        // Directory order follows the dyntuple's stored fields for every codec,
        // including immutable origins; fitting order is independent of storage.
        assert!(
            column.range().start >= end && column.range().end <= frame.unit,
            "invalid column layout"
        );
        end = column.range().end;
        if column.is_unsigned() {
            let footer = &fixed[fixed_index];
            fixed_index += 1;
            let padded = record_storage_bytes(footer.len());
            sizes.push((padded >> 1) as u8);
            records.extend_from_slice(footer);
            records.resize(records.len() + padded - footer.len(), 0);
            pending -= padded;
            continue;
        }
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
            // Mandatory unsigned fields are fitted separately. Their directory
            // slots must not change legacy alignment tie breaks for selectable codecs.
            trailer.len() + records.len() + pending + layout.len() - fixed_count,
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
            + record_storage_bytes(selected.record.len())
            + pending
            + layout.len()
            <= u16::MAX as usize;
        if !selected.record.is_empty() && fits {
            rewrite(frame, column, |_, row, _| selected.encoded[row]);
            let padded = record_storage_bytes(selected.record.len());
            sizes.push((padded >> 1) as u8);
            let end = records.len() + padded;
            records.extend_from_slice(&selected.record);
            records.resize(end, 0);
        } else {
            sizes.push(0);
        }
    }
    // Serialize records backward in schema order: entry zero starts at the
    // directory boundary, so its address depends only on its own size byte.
    let mut end = records.len();
    for &size in sizes.iter().rev() {
        let start = end - usize::from(size) * 2;
        trailer.extend_from_slice(&records[start..end]);
        end = start;
    }
    trailer.extend_from_slice(&sizes);
}

#[cfg(test)]
mod tests;

// Inspection is compiled out by default and never instruments the recording path.

/// Common transient inputs. Encodings own their footer bytes; the column supplies
/// row geometry and the immutable physical first word only when needed.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub frame: &'a ArchivedFrame,
    pub layout: Layout,
}
impl Context<'_> {
    #[inline]
    pub fn width(self) -> usize {
        usize::from(self.layout.width)
    }
    #[inline]
    pub fn rows(self) -> usize {
        self.frame.len()
    }
    #[inline]
    pub fn first(self) -> u128 {
        read_word(&self.frame.first()[self.layout.range()])
    }
}
/// A column's storage access is independent of its statically dispatched codec.
pub struct Column<'a, E: Encoding<'a> = AnyEncoding<'a>> {
    frame: &'a ArchivedFrame,
    layout: Layout,
    encoding: E,
}
impl<'a, E: Encoding<'a>> Column<'a, E> {
    pub fn with_encoding(frame: &'a ArchivedFrame, layout: Layout, encoding: E) -> Self {
        if let Some(width) = E::WIDTH {
            assert_eq!(usize::from(layout.width), width);
        }
        assert!(
            layout.range().end <= frame.unit(),
            "column exceeds frame row"
        );
        Self {
            frame,
            layout,
            encoding,
        }
    }
    pub fn parse(frame: &'a ArchivedFrame, layout: Layout, record: &'a [u8]) -> Self {
        Self::with_encoding(frame, layout, E::parse(record, layout, frame.len()))
    }
    #[inline]
    fn context(&self) -> Context<'_> {
        Context {
            frame: self.frame,
            layout: self.layout,
        }
    }
    #[inline]
    pub fn decode(&self, encoded: u128, row: usize) -> u128 {
        self.encoding.decode(encoded, row, self.context())
    }
    #[inline]
    pub fn encode(&self, value: u128, row: usize) -> Option<u128> {
        self.encoding.encode(value, row, self.context())
    }
    #[inline]
    pub(crate) fn decode_row(&self, row: &mut [u8], index: usize) {
        let start = usize::from(self.layout.offset);
        let width = E::WIDTH.unwrap_or(usize::from(self.layout.width));
        let range = start..start + width;
        let value = self.decode(read_word(&row[range.clone()]), index);
        write_word(&mut row[range], value);
    }
    pub fn encoding(&self) -> &E {
        &self.encoding
    }
    pub fn read(&self, index: usize) -> u128 {
        let mut bytes = [0; 16];
        let width = E::WIDTH.unwrap_or(usize::from(self.layout.width));
        self.frame
            .read_column(index, usize::from(self.layout.offset), &mut bytes[..width]);
        self.decode(read_word(&bytes[..width]), index)
    }
}

impl<'a> Column<'a> {
    pub fn new(frame: &'a ArchivedFrame, layout: Layout, record: &'a [u8]) -> Self {
        Self::parse(frame, layout, record)
    }
    pub fn is_dictionary(&self) -> bool {
        matches!(
            self.encoding,
            AnyEncoding::Dictionary(_) | AnyEncoding::SplitDictionary(_)
        )
    }
}
