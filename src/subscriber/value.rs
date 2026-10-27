//! CODEX GENERATED
//! Owned subscriber inputs: Debug/Display are rendered once, before template matching.
use introspect_fmt::{ArgumentKind, CaptureSink, Format, Recipe, RenderError, Slots, Value};
use smallvec::SmallVec;
use tracing_core::{Field, field::Visit};

/// Portable values preserve numeric bits and distinguish text from absent fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InputValue {
    // Float snapshots use raw bits so NaNs and signed zero remain lossless.
    I8(i8),
    I16(i16),
    I32(i32),
    Isize(isize),
    U8(u8),
    U16(u16),
    U32(u32),
    Usize(usize),
    F32(u32),
    Char(char),

    F64(u64),
    I64(i64),
    U64(u64),
    I128(i128),
    U128(u128),
    Bool(bool),
    Text(String),
    Bytes(Vec<u8>),
}
impl InputValue {
    /// Logical payload size excludes MessagePack framing and field/callsite metadata.
    pub fn bytes(&self) -> usize {
        match self {
            Self::I8(_) => size_of::<i8>(),
            Self::I16(_) => size_of::<i16>(),
            Self::I32(_) => size_of::<i32>(),
            Self::Isize(_) => size_of::<isize>(),
            Self::U8(_) => size_of::<u8>(),
            Self::U16(_) => size_of::<u16>(),
            Self::U32(_) => size_of::<u32>(),
            Self::Usize(_) => size_of::<usize>(),
            Self::F32(_) => size_of::<f32>(),
            Self::Char(_) => size_of::<char>(),
            Self::F64(_) | Self::I64(_) | Self::U64(_) => 8,
            Self::I128(_) | Self::U128(_) => 16,
            Self::Bool(_) => 1,
            Self::Text(v) => v.len(),
            Self::Bytes(v) => v.len(),
        }
    }
}

/// One formatted field's snapshot. Names exist only to serialize new recipes;
/// static templates have already resolved them to argument indices.
pub(super) struct CapturedArguments {
    pub field: usize,
    pub recipe: Recipe,
    pub names: SmallVec<[Option<&'static str>; 4]>,
    pub values: Vec<InputValue>,
}

pub(super) struct InputVisitor {
    pub values: Vec<Option<InputValue>>,
    pub is_log: bool,
    pub(super) formats: Vec<CapturedArguments>,
}
impl InputVisitor {
    pub fn new(count: usize, is_log: bool) -> Self {
        Self {
            values: vec![None; count],
            is_log,
            formats: Vec::new(),
        }
    }
    // Replay owned snapshots, never re-enter the original user's Display/Debug.
    pub(super) fn record_into(
        &self,
        metadata: &tracing::Metadata<'_>,
        visitor: &mut super::ValueRecorder<'_>,
    ) {
        visit_values(metadata, &self.values, visitor, |index| {
            self.formats.iter().any(|format| format.field == index)
        });
        for format in &self.formats {
            let field = metadata.fields().iter().nth(format.field).unwrap();
            visitor.record_inputs(&field, &format.recipe, &format.values);
        }
    }
    fn set(&mut self, field: &Field, value: InputValue) {
        // Normalized log metadata has just `message`; log.* fields belong to metadata.
        if !self.is_log || field.name() == "message" {
            self.values[field.index()] = Some(value);
        }
    }
}
impl Visit for InputVisitor {
    fn visit_format(&mut self, field: &Field, value: &tracing_core::field::Format<'_>) {
        // Portable capture always keeps recipes; choosing rendered input is a replay policy.
        let recipe = Recipe::new(value).expect("compiling input format");
        let values = capture_inputs(&recipe, value).expect("capturing input format");
        let text = render_inputs(&recipe, &values).expect("rendering captured input");
        self.set(field, InputValue::Text(text));
        self.formats.push(CapturedArguments {
            field: field.index(),
            recipe,
            names: value.args().iter().map(|arg| arg.name()).collect(),
            values,
        });
    }

    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        self.set(f, InputValue::Text(format!("{v:?}")));
    }
    fn record_str(&mut self, f: &Field, v: &str) {
        self.set(f, InputValue::Text(v.to_owned()));
    }
    fn record_f64(&mut self, f: &Field, v: f64) {
        self.set(f, InputValue::F64(v.to_bits()));
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.set(f, InputValue::I64(v));
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.set(f, InputValue::U64(v));
    }
    fn record_i128(&mut self, f: &Field, v: i128) {
        self.set(f, InputValue::I128(v));
    }
    fn record_u128(&mut self, f: &Field, v: u128) {
        self.set(f, InputValue::U128(v));
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        self.set(f, InputValue::Bool(v));
    }
    fn record_bytes(&mut self, f: &Field, v: &[u8]) {
        self.set(f, InputValue::Bytes(v.to_owned()));
    }
    fn record_error(&mut self, f: &Field, v: &(dyn std::error::Error + 'static)) {
        // Match ValueRecorder's error rendering, including string Debug quoting.
        self.record_debug(f, &v.to_string());
    }
}

// used for replay
pub(super) fn visit_values(
    metadata: &tracing::Metadata<'_>,
    values: &[Option<InputValue>],
    visitor: &mut dyn Visit,
    skip: impl Fn(usize) -> bool,
) {
    for (field, value) in metadata.fields().iter().zip(values) {
        // Native Format fields bypass rendered matching and temporary text copies.
        if skip(field.index()) {
            continue;
        }
        match value {
            None => {}
            // Ordinary Visit methods normalize widths; format arguments use
            // record_format directly and never pass through this boundary.
            Some(InputValue::I8(v)) => visitor.record_i64(&field, *v as i64),
            Some(InputValue::I16(v)) => visitor.record_i64(&field, *v as i64),
            Some(InputValue::I32(v)) => visitor.record_i64(&field, *v as i64),
            Some(InputValue::Isize(v)) => visitor.record_i64(&field, *v as i64),
            Some(InputValue::U8(v)) => visitor.record_u64(&field, *v as u64),
            Some(InputValue::U16(v)) => visitor.record_u64(&field, *v as u64),
            Some(InputValue::U32(v)) => visitor.record_u64(&field, *v as u64),
            Some(InputValue::Usize(v)) => visitor.record_u64(&field, *v as u64),
            Some(InputValue::F32(v)) => visitor.record_f64(&field, f64::from(f32::from_bits(*v))),
            Some(InputValue::Char(v)) => visitor.record_str(&field, &v.to_string()),
            Some(InputValue::F64(v)) => visitor.record_f64(&field, f64::from_bits(*v)),
            Some(InputValue::I64(v)) => visitor.record_i64(&field, *v),
            Some(InputValue::U64(v)) => visitor.record_u64(&field, *v),
            Some(InputValue::I128(v)) => visitor.record_i128(&field, *v),
            Some(InputValue::U128(v)) => visitor.record_u128(&field, *v),
            Some(InputValue::Bool(v)) => visitor.record_bool(&field, *v),
            Some(InputValue::Text(v)) => visitor.record_str(&field, v),
            Some(InputValue::Bytes(v)) => visitor.record_bytes(&field, v),
        }
    }
}

// ---- Format recipes and portable values ----
// Floats keep their bits; text is the only non-primitive slot. Opaque arguments
// never appear here: their requested output is captured as text.

/// Snapshot a format's arguments for portable dumps and comparison replay.
pub(super) fn capture_inputs(
    recipe: &Recipe,
    format: &Format<'_>,
) -> Result<Vec<InputValue>, RenderError> {
    let mut values = InputValues(Vec::with_capacity(recipe.value_count()));
    recipe.capture(format, &mut values)?;
    Ok(values.0)
}
/// Validate portable arguments before a replay can reach schema assertions.
pub(super) fn accepts_inputs(recipe: &Recipe, values: &[InputValue]) -> bool {
    let mut kinds = recipe.slot_kinds();
    values
        .iter()
        .all(|value| kinds.next().is_some_and(|kind| accepts_kind(kind, value)))
        && kinds.next().is_none()
}
pub(super) fn render_inputs<T: AsInput>(
    recipe: &Recipe,
    values: &[T],
) -> Result<String, RenderError> {
    recipe.render(&InputSlots(values))
}

macro_rules! conversions {
    ($($variant:ident => $input:ident, $encode:expr, $decode:expr);* $(;)?) => {
        fn accepts_kind(kind: ArgumentKind, value: &InputValue) -> bool {
            matches!((kind, value), $((ArgumentKind::$variant, InputValue::$input(_)))|*)
        }
        /// Opaque values have no input representation; their text is captured instead.
        fn input_value(value: &Value<'_>) -> Option<InputValue> {
            Some(match value {
                $(Value::$variant(value) => InputValue::$input(($encode)(*value)),)*
                Value::Opaque(_) => return None,
            })
        }
        fn slot_value(value: &InputValue) -> Option<Value<'_>> {
            Some(match value {
                $(InputValue::$input(value) => Value::$variant(($decode)(value)),)*
                _ => return None,
            })
        }
    };
}
conversions! {
    Bool => Bool, |v: &bool| *v, |v| v;
    Char => Char, |v: &char| *v, |v| v;
    I8 => I8, |v: &i8| *v, |v| v;
    I16 => I16, |v: &i16| *v, |v| v;
    I32 => I32, |v: &i32| *v, |v| v;
    I64 => I64, |v: &i64| *v, |v| v;
    I128 => I128, |v: &i128| *v, |v| v;
    Isize => Isize, |v: &isize| *v, |v| v;
    U8 => U8, |v: &u8| *v, |v| v;
    U16 => U16, |v: &u16| *v, |v| v;
    U32 => U32, |v: &u32| *v, |v| v;
    U64 => U64, |v: &u64| *v, |v| v;
    U128 => U128, |v: &u128| *v, |v| v;
    Usize => Usize, |v: &usize| *v, |v| v;
    // Floats are stored as bits; the same-layout view renders without copying.
    F32 => F32, |v: &f32| v.to_bits(), bytemuck::cast_ref::<u32, f32>;
    F64 => F64, |v: &f64| v.to_bits(), bytemuck::cast_ref::<u64, f64>;
    Str => Text, str::to_owned, String::as_str;
}

struct InputValues(Vec<InputValue>);
impl CaptureSink for InputValues {
    fn value(&mut self, value: &Value<'_>) {
        self.0.extend(input_value(value));
    }
    fn text(&mut self) -> &mut String {
        self.0.push(InputValue::Text(String::new()));
        let Some(InputValue::Text(text)) = self.0.last_mut() else {
            unreachable!()
        };
        text
    }
}

/// Stored rows hold optional fields; captured snapshots hold plain values.
pub(super) trait AsInput {
    fn as_input(&self) -> Option<&InputValue>;
}
impl AsInput for InputValue {
    fn as_input(&self) -> Option<&InputValue> {
        Some(self)
    }
}
impl AsInput for Option<InputValue> {
    fn as_input(&self) -> Option<&InputValue> {
        self.as_ref()
    }
}

struct InputSlots<'a, T>(&'a [T]);
impl<T: AsInput> Slots for InputSlots<'_, T> {
    fn slot(&self, index: usize) -> Option<Value<'_>> {
        slot_value(self.0.get(index)?.as_input()?)
    }
}
