//! CODEX GENERATED
//! Owned subscriber inputs: Debug/Display are rendered once, before template matching.
use tracing_core::{Field, field::Visit};

/// Portable values preserve numeric bits and distinguish text from absent fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InputValue {
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
            Self::F64(_) | Self::I64(_) | Self::U64(_) => 8,
            Self::I128(_) | Self::U128(_) => 16,
            Self::Bool(_) => 1,
            Self::Text(v) => v.len(),
            Self::Bytes(v) => v.len(),
        }
    }
}

pub(super) struct InputVisitor {
    pub values: Vec<Option<InputValue>>,
    pub is_log: bool,
}
impl InputVisitor {
    pub fn new(count: usize, is_log: bool) -> Self {
        Self {
            values: vec![None; count],
            is_log,
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
) {
    for (field, value) in metadata.fields().iter().zip(values) {
        match value {
            None => {}
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
