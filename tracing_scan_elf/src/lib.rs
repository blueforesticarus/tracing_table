//! Scan tracing and log callsites and load their CSV or embedded ELF metadata.
//! Extraction, source lookup, and artifact formats live in separate modules.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

mod artifacts;
mod dwarf;
mod source;
mod static_metadata;

pub use artifacts::{
    SECTION_NAME, load_csv, load_elf_section, load_embedded_section, read_csv, read_section,
    write_csv, write_raw_section, write_section,
};
pub use dwarf::extract;
pub use source::{lookup_templates, parse_source_templates};

// ---- Shared types ----------------------------------------------------------

/// I/O, DWARF, and format errors; individual template failures live in `Callsite`.
pub type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

/// A literal format string plus Rust expressions with normalized token spacing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Template {
    pub format: String,
    /// Decimal position or argument name -> Rust expression, including captures.
    pub args: BTreeMap<String, String>,
    /// Field name -> expression, preserving tracing ?/% or log capture modifiers.
    pub fields: BTreeMap<String, String>,
}

// Accept legacy arrays while writing only dictionaries in new artifacts.
#[derive(Deserialize)]
#[serde(untagged)]
enum Expressions {
    Dictionary(BTreeMap<String, String>),
    Legacy(Vec<String>),
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            format: String,
            args: Expressions,
            fields: Expressions,
        }
        let stored = Stored::deserialize(deserializer)?;
        source::decode_template(stored.format, stored.args, stored.fields)
            .map_err(serde::de::Error::custom)
    }
}

/// A source lookup failure retained as data rather than aborting the scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupError {
    /// Original source line, when available (not its line number).
    pub line: String,
    pub error: String,
}

/// Callsite classification retained even when template lookup is skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallsiteKind {
    Event,
    Span,
    Log,
}

const NO_MESSAGE: &str = "template lookup skipped: event has no message field";
const SPAN_TEMPLATE: &str = "template lookup skipped: span names are not format strings";
const SOURCE_NOT_EXAMINED: &str = "source not examined";

/// One logging location, read from tracing metadata or log's DWARF marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callsite {
    /// Canonical tracing metadata filename, or full DWARF source path for log.
    pub file: String,
    pub line: u32,
    /// True for `log` macros; false for `tracing` events and spans.
    pub is_log: bool,
    /// Serialized as the `type` column/field: event, span, or log.
    pub kind: CallsiteKind,
    /// Canonical tracing metadata module, or enclosing DWARF namespaces for log.
    /// Never inferred from filenames or Cargo package directories.
    pub module_path: Option<String>,
    /// Zero-based table row containing the wrapper's declared logging location.
    /// None for ordinary sites. CSV row_id uses the same numbering.
    pub wrapper_row: Option<usize>,
    /// Path used for source lookup; artifact readers initialize it from `file`.
    pub source: PathBuf,
    /// Original source line, independently of whether template parsing succeeds.
    pub source_line: String,
    /// Full macro invocation, including newlines; falls back to source_line.
    pub macro_call: String,
    pub template: Result<Template, LookupError>,
    // Extraction evidence, not runtime metadata. Scope declarations constrain
    // source recovery to functions that actually contain this DWARF marker.
    scopes: BTreeSet<SourceLocation>,
    // Macro definition evidence can differ from tracing's canonical invocation.
    declaration: Option<SourceLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SourceLocation {
    file: String,
    line: u32,
}

impl Callsite {
    // Both DWARF matchers start with the same pending source-lookup result.
    fn unresolved(file: String, line: u32, is_log: bool, module_path: Option<String>) -> Self {
        Self::loaded(
            file,
            line,
            is_log,
            module_path,
            Err(LookupError {
                line: String::new(),
                error: SOURCE_NOT_EXAMINED.into(),
            }),
        )
    }

    // Both readers reconstruct the same source-path fallback from stored data.
    fn loaded(
        file: String,
        line: u32,
        is_log: bool,
        module_path: Option<String>,
        template: Result<Template, LookupError>,
    ) -> Self {
        let source_line = template
            .as_ref()
            .err()
            .map_or_else(String::new, |e| e.line.clone());
        Self {
            source: PathBuf::from(&file),
            file,
            line,
            is_log,
            kind: if is_log {
                CallsiteKind::Log
            } else {
                CallsiteKind::Event
            },
            macro_call: source_line.clone(),
            source_line,
            module_path,
            wrapper_row: None,
            template,
            scopes: BTreeSet::new(),
            declaration: None,
        }
    }

    fn skip_template(&self) -> bool {
        self.kind == CallsiteKind::Span
            || matches!(&self.template, Err(error) if error.error == NO_MESSAGE)
    }
}

#[cfg(test)]
mod tests;
