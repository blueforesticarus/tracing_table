//! Runtime lookup from vendored records; no source files or scanner artifacts.
use super::CompiledTemplate;
use std::{collections::HashMap, sync::LazyLock};
use tracing::{Metadata, callsite::Identifier};
use tracing_core::callsite_table::{self, Entry};

// Build once, then resolve each native callsite by identity rather than location.
static RECORDS: LazyLock<HashMap<Identifier, &'static Entry>> = LazyLock::new(|| {
    callsite_table::records()
        .map(|record| (record.metadata().unwrap().callsite(), record))
        .collect()
});

pub(super) fn lookup(
    metadata: &'static Metadata<'static>,
    is_log: bool,
) -> Option<CompiledTemplate> {
    let format = if is_log {
        // Dynamic log-backend calls are adapted into synthetic tracing metadata.
        // Their separate vendor table retains location and template; do not pick
        // an arbitrary format if several records at that location disagree.
        let mut rows = callsite_table::log_fallback_records().filter(|record| {
            record.file() == metadata.file()
                && Some(record.line()) == metadata.line()
                && Some(record.module_path()) == metadata.module_path()
        });
        let format = rows.next()?.template();
        if !rows.all(|record| record.template() == format) {
            return None;
        }
        format?
    } else {
        RECORDS.get(&metadata.callsite())?.template()?
    };
    CompiledTemplate::compile(format).ok()
}
