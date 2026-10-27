//! Flat codec families and their shared interface for borrowed columns.
use super::column::{Context, Layout};
use common::{DICTIONARY, DICTIONARY_CYCLE, DICTIONARY_STRIDE};
use smallvec::SmallVec;

pub(crate) mod common;
pub mod dictionary;
pub mod exceptions;
pub mod origin;
pub mod prediction;
pub mod raw;
pub mod reference;
pub mod split_dictionary;

pub use dictionary::DictionaryEncoding;
pub use exceptions::ExceptionsEncoding;
pub use origin::OriginEncoding;
pub use prediction::PredictionEncoding;
pub use raw::RawEncoding;
pub use split_dictionary::SplitDictionaryEncoding;

pub use common::{
    MAX_CYCLE, MAX_CYCLE_CANDIDATES, MAX_DICTIONARY_VALUES, MAX_RECORD_BYTES, MIN_ENCODING_SAVINGS,
    UNSIGNED_MASK_CANDIDATES, UNSIGNED_MASK_STRIDE, UNSIGNED_MINIMAX_STRIDE,
};

pub type Footer = SmallVec<[u8; 24]>;

/// Implementations are flat codec families. This interface adds no archived
/// fields, trait objects or allocations; runtime selection uses AnyEncoding.
pub trait Encoding<'a>: Sized {
    /// Fixed-width codecs let typed columns keep native word reads in the hot loop.
    const WIDTH: Option<usize> = None;
    fn parse(record: &'a [u8], layout: Layout, rows: usize) -> Self;
    fn write_footer(&self, width: usize, output: &mut Footer);
    fn encode(&self, value: u128, row: usize, context: Context) -> Option<u128>;
    fn decode(&self, encoded: u128, row: usize, context: Context) -> u128;
}

/// Runtime archives remain heterogeneous; each match arm calls a concrete impl.
pub enum AnyEncoding<'a> {
    Raw(RawEncoding),
    Prediction(PredictionEncoding),
    Unsigned(PredictionEncoding<true>),
    Origin(OriginEncoding),
    Dictionary(DictionaryEncoding<'a>),
    Exceptions(ExceptionsEncoding<'a, true>),
    SplitDictionary(SplitDictionaryEncoding<'a>),
}
// Keep forwarding in one place; codec implementations never contain each other.
macro_rules! dispatch_encoding {
    ($self:expr, $binding:ident => $body:expr) => {
        match $self {
            AnyEncoding::Raw($binding) => $body,
            AnyEncoding::Prediction($binding) => $body,
            AnyEncoding::Unsigned($binding) => $body,
            AnyEncoding::Origin($binding) => $body,
            AnyEncoding::Dictionary($binding) => $body,
            AnyEncoding::Exceptions($binding) => $body,
            AnyEncoding::SplitDictionary($binding) => $body,
        }
    };
}
impl<'a> Encoding<'a> for AnyEncoding<'a> {
    fn parse(record: &'a [u8], layout: Layout, rows: usize) -> Self {
        if layout.is_tagged_origin() {
            if SplitDictionaryEncoding::is_tagged(record) {
                return Self::SplitDictionary(SplitDictionaryEncoding::parse(record, layout, rows));
            }
            return Self::Origin(OriginEncoding::parse(record, layout, rows));
        }
        if layout.is_unsigned() && !record.is_empty() {
            return Self::Unsigned(<PredictionEncoding<true> as Encoding>::parse(
                record, layout, rows,
            ));
        }
        match record.first().copied() {
            None => Self::Raw(RawEncoding),
            Some(exceptions::TAG) => {
                Self::Exceptions(ExceptionsEncoding::parse(record, layout, rows))
            }
            Some(split_dictionary::TAG) => {
                Self::SplitDictionary(SplitDictionaryEncoding::parse(record, layout, rows))
            }
            Some(DICTIONARY | DICTIONARY_CYCLE | DICTIONARY_STRIDE) => {
                Self::Dictionary(DictionaryEncoding::parse(record, layout, rows))
            }
            _ => Self::Prediction(<PredictionEncoding as Encoding>::parse(
                record, layout, rows,
            )),
        }
    }
    fn write_footer(&self, width: usize, output: &mut Footer) {
        dispatch_encoding!(self, e => e.write_footer(width, output));
    }
    #[inline]
    fn encode(&self, value: u128, row: usize, context: Context) -> Option<u128> {
        dispatch_encoding!(self, e => e.encode(value, row, context))
    }
    #[inline]
    fn decode(&self, encoded: u128, row: usize, context: Context) -> u128 {
        dispatch_encoding!(self, e => e.decode(encoded, row, context))
    }
}

impl AnyEncoding<'_> {
    /// Timestamp callers inspect the common header without depending on which
    /// flat encoding owns its row representation. No expanded rows are cached.
    pub fn origin(&self) -> u64 {
        match self {
            Self::Origin(e) => e.origin(),
            Self::SplitDictionary(e) => e.origin(),
            _ => panic!("encoding has no tagged origin"),
        }
    }
    pub fn is_relative(&self) -> bool {
        match self {
            Self::Origin(e) => e.is_relative(),
            Self::SplitDictionary(e) => e.is_relative(),
            _ => false,
        }
    }
    pub fn stride_float(&self) -> crate::data::util::float_u16::FloatU16 {
        match self {
            Self::Origin(e) => e.stride_float(),
            Self::SplitDictionary(e) => e.stride_float(),
            _ => crate::data::util::float_u16::FloatU16::default(),
        }
    }
    pub fn is_split_dictionary(&self) -> bool {
        matches!(self, Self::SplitDictionary(_))
    }
}
