//! Callsite-owned formatting recipes and conversion to the existing packed values.
//! Preserve Rust primitive identity and native payload width through storage.
use super::InputValue;
use introspect_fmt::{Format as BorrowedFormat, OwnedTemplate, OwnedValue, RenderError};

macro_rules! argument_kinds {
    ($($variant:ident => $input:ident, $encode:expr, $decode:expr);* $(;)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) enum ArgumentKind { $($variant,)* }
        impl ArgumentKind {
            pub(super) fn tag(self) -> &'static str {
                match self { $(Self::$variant => stringify!($variant),)* }
            }
            pub(super) fn from_tag(tag: &str) -> Option<Self> {
                match tag { $(stringify!($variant) => Some(Self::$variant),)* _ => None }
            }
            pub(super) fn accepts(self, value: &InputValue) -> bool {
                matches!((self, value), $((Self::$variant, InputValue::$input(_)))|*)
            }
            fn capture(value: OwnedValue) -> (Self, InputValue) {
                match value {
                    $(OwnedValue::$variant(value) => (Self::$variant, InputValue::$input(($encode)(value))),)*
                }
            }
            fn restore(self, value: &InputValue) -> OwnedValue {
                match (self, value) {
                    $((Self::$variant, InputValue::$input(value)) => OwnedValue::$variant(($decode)(value)),)*
                    _ => panic!("format argument disagrees with callsite schema"),
                }
            }
        }
    };
}
argument_kinds! {
    Bool => Bool, |v| v, |v: &bool| *v;
    Char => Char, |v| v, |v: &char| *v;
    I8 => I8, |v| v, |v: &i8| *v;
    I16 => I16, |v| v, |v: &i16| *v;
    I32 => I32, |v| v, |v: &i32| *v;
    I64 => I64, |v| v, |v: &i64| *v;
    I128 => I128, |v| v, |v: &i128| *v;
    Isize => Isize, |v| v, |v: &isize| *v;
    U8 => U8, |v| v, |v: &u8| *v;
    U16 => U16, |v| v, |v: &u16| *v;
    U32 => U32, |v| v, |v: &u32| *v;
    U64 => U64, |v| v, |v: &u64| *v;
    U128 => U128, |v| v, |v: &u128| *v;
    Usize => Usize, |v| v, |v: &usize| *v;
    F32 => F32, f32::to_bits, |v: &u32| f32::from_bits(*v);
    F64 => F64, f64::to_bits, |v: &u64| f64::from_bits(*v);
    Text => Text, |v| v, |v: &String| v.clone();
}

/// Templates and original primitive types belong to the callsite, never the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    pub template: OwnedTemplate,
    pub(super) kinds: Vec<ArgumentKind>,
}
impl Format {
    /// Validate portable arguments before a replay can reach schema assertions.
    pub(super) fn accepts(&self, values: &[InputValue]) -> bool {
        self.kinds.len() == values.len()
            && self
                .kinds
                .iter()
                .zip(values)
                .all(|(kind, value)| kind.accepts(value))
    }
    /// Heap capacity excludes this inline value and its enclosing Arc allocation.
    pub fn heap_bytes(&self) -> usize {
        self.template.heap_bytes() + self.kinds.capacity() * size_of::<ArgumentKind>()
    }
    pub(super) fn capture_new(
        value: &BorrowedFormat<'_>,
    ) -> Result<(Self, Vec<InputValue>), RenderError> {
        let template = OwnedTemplate::compile(value)?;
        let (kinds, values) = template
            .capture(value)?
            .into_iter()
            .map(ArgumentKind::capture)
            .unzip();
        Ok((Self { template, kinds }, values))
    }
    pub(super) fn capture(
        &self,
        value: &BorrowedFormat<'_>,
    ) -> Result<Vec<InputValue>, RenderError> {
        Ok(self
            .template
            .capture(value)?
            .into_iter()
            .map(|value| ArgumentKind::capture(value).1)
            .collect())
    }
    pub fn render(&self, values: &[Option<InputValue>]) -> Result<String, RenderError> {
        assert_eq!(values.len(), self.kinds.len());
        let values: Vec<_> = self
            .kinds
            .iter()
            .zip(values)
            .map(|(kind, value)| kind.restore(value.as_ref().expect("format argument")))
            .collect();
        self.template.render(&values)
    }
}
