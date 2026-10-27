//! Opt-in, post-recording storage leaves. No counters, summaries, or instrumentation
//! are retained by columns. Consumers choose grouping and normalization themselves.
use crate::data::column::read_word;
use crate::data::column::*;
use crate::data::encoding::common::{
    CYCLE, DICTIONARY, DICTIONARY_CYCLE, DICTIONARY_STRIDE, LINEAR, OFFSET, ROW, ROW_CYCLE,
};
use crate::subscriber::{Archetype, RecordingLayer};
use smallvec::SmallVec;
use std::{
    collections::HashMap,
    io::{self, Write},
    ops::Range,
};
use streaming_iterator::StreamingIterator;
mod layouts;
pub use layouts::write_layouts;

#[derive(Clone)]
struct Field {
    name: String,
    role: &'static str,
    primitive: String,
    range: Range<usize>,
    raw_width: usize,
    record: Option<usize>,
    stored: Option<Range<usize>>,
    alias: Option<usize>,
}

// All reference columns have a 16-byte logical baseline and 10-byte physical row.
fn is_reference(name: &str) -> bool {
    matches!(name, "parent_span_id" | "update_span_id" | "follows_from")
}

fn fields(arch: &Archetype, names: &[String]) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut offset = 0;
    let mut structural = |name: &str, width, raw_width| {
        fields.push(Field {
            name: name.into(),
            role: "structural",
            primitive: name.into(),
            range: offset..offset + width,
            raw_width,
            record: None,
            stored: Some(offset..offset + width),
            alias: None,
        });
        offset += width;
    };
    if arch.is_span_end {
        structural("update_span_id", 10, 16);
    } else {
        structural("start_ts", 8, 8);
        if arch.is_event {
            structural("timestamp_index", 2, 2);
        } else {
            structural("end_duration", 8, 8);
        }
    }
    if arch.parent {
        structural("parent_span_id", 10, 16);
    }
    if arch.follows_from {
        structural("follows_from", 10, 16);
    }
    if let Some(start) = fields.iter_mut().find(|f| f.name == "start_ts") {
        start.record = Some(0);
    }
    // Duration follows the ordinary timestamp column, ahead of ordinary payload fields.
    if let Some(duration) = fields.iter_mut().find(|field| field.name == "end_duration") {
        duration.record = Some(1);
    }
    for (index, kind) in arch.field_types.iter().enumerate() {
        let Some(kind) = kind else {
            continue;
        };
        let argument = arch
            .formats
            .iter()
            .filter(|(_, _, start)| *start <= index)
            .max_by_key(|(_, _, start)| *start);
        let (name, role) = if let Some(&(field, _, start)) = argument {
            (
                format!("arg.{}[{}]", names[field], index - start),
                "format_arg",
            )
        } else {
            (format!("field.{}", names[index]), "field")
        };
        let record = Some(fields.iter().filter(|field| field.record.is_some()).count());
        fields.push(Field {
            name,
            role,
            primitive: format!("{kind:?}"),
            range: offset..offset + kind.unit(),
            raw_width: kind.unit(),
            record,
            stored: Some(offset..offset + kind.unit()),
            alias: None,
        });
        offset += kind.unit();
    }
    for argument in 0..arch.template.unwrap_or(0) {
        let width = arch.string_storage.unit();
        let record = Some(fields.iter().filter(|field| field.record.is_some()).count());
        fields.push(Field {
            name: format!("arg.message[{argument}]"),
            role: "template_arg",
            primitive: "BufferIndex".into(),
            range: offset..offset + width,
            raw_width: width,
            record,
            stored: Some(offset..offset + width),
            alias: None,
        });
        offset += width;
    }
    assert_eq!(offset, arch.unit());
    fields
}

/// Every CSV row is one storage component of one column in one archived block.
/// `values` counts logical observations only on first/tail rows, so it is additive.
/// Bytes/value, bytes/column, saved bytes, percentages, and totals are NOT stored.
pub fn write_recording(
    mut output: impl Write,
    run: &str,
    recorder: &RecordingLayer,
    ids: Option<&HashMap<usize, u64>>,
) -> io::Result<()> {
    writeln!(
        output,
        "run,callsite,archetype,block_index,column,role,primitive,encoding,component,entry,values,retained_bits,raw_bitcompact_bits,uncompacted_bits,target,file,line,name"
    )?;
    let sites = recorder.callsites.read();
    for (key, shared) in sites.iter() {
        let site = shared.read();
        let callsite = ids.map_or(site.id, |mapping| mapping[key]);
        let names: Vec<_> = site
            .metadata
            .fields()
            .iter()
            .map(|field| field.name().to_owned())
            .collect();
        for arch in &site.cache {
            let fallback = arch.archetype.column_layout();
            let logical_fields = fields(&arch.archetype, &names);
            for (ordinal, block) in arch.data.ring.iter().enumerate() {
                let projection = arch.data.layout_for_block(arch.data.offset + ordinal);
                let layout = arch
                    .data
                    .columns_for_block(arch.data.offset + ordinal, &fallback);
                let fields: Vec<_> = logical_fields
                    .iter()
                    .cloned()
                    .map(|mut field| {
                        field.record = field
                            .stored
                            .as_ref()
                            .and_then(|range| layout.iter().position(|c| c.range() == *range));
                        if let Some(p) = projection {
                            field.stored = p.physical_range(field.range.clone());
                            field.record = field
                                .stored
                                .as_ref()
                                .and_then(|range| layout.iter().position(|c| c.range() == *range));
                            if field.stored.is_none() {
                                let f = p
                                    .fields
                                    .iter()
                                    .find(|f| f.logical.range() == field.range)
                                    .unwrap();
                                let source = p.fields[usize::from(f.source)].logical.range();
                                field.alias = logical_fields.iter().position(|f| f.range == source);
                            }
                        }
                        field
                    })
                    .collect();
                let frame = &block.data.frame;
                let records: Vec<_> = block
                    .data
                    .column_metadata(layout)
                    .entries()
                    .map(|(_, record)| record)
                    .collect();
                let parents = block.data.parent_offsets(layout);
                let mut first = vec![0; fields.len()];
                let mut masks = vec![0u128; fields.len()];
                let mut rows =
                    crate::data::dataframe::ProjectedIter::new(block.data.iter(layout), projection);
                let mut row_index = 0;
                while let Some(row) = rows.next() {
                    for (index, field) in fields.iter().enumerate() {
                        let value = match field.name.as_str() {
                            "start_ts" if block.data.timestamp_parent_relative(layout) => {
                                let offset = if arch.archetype.is_event { 10 } else { 16 };
                                let bytes = &row[offset..offset + 10];
                                let entry = parents
                                    .entry(u16::from_ne_bytes(bytes[8..].try_into().unwrap()));
                                let parent = crate::subscriber::SpanId {
                                    callsite:
                                        crate::subscriber::storage::CallsiteArchetype::from_bits(
                                            entry.identity,
                                        )
                                        .callsite(),
                                    archetype:
                                        crate::subscriber::storage::CallsiteArchetype::from_bits(
                                            entry.identity,
                                        )
                                        .archetype(),
                                    index: entry.decode_index(
                                        read_word(&bytes[..8]) as u64,
                                        row_index,
                                        frame.len(),
                                    ),
                                };
                                let elapsed = read_word(&row[field.range.clone()]) as u64;
                                // Missing ancestors are allowed; retain a usable
                                // elapsed baseline when absolute time is lost.
                                u128::from(
                                    site.span_timestamp(parent)
                                        .and_then(|v| v.checked_add(elapsed))
                                        .unwrap_or(elapsed),
                                )
                            }
                            "start_ts" => read_word(&row[field.range.clone()]),
                            "parent_span_id" | "update_span_id" | "follows_from" => {
                                let bytes = &row[field.range.clone()];
                                let entry = parents
                                    .entry(u16::from_ne_bytes(bytes[8..].try_into().unwrap()));
                                // The uncompacted baseline is the original 16-byte SpanId.
                                let mut raw = [0; 16];
                                raw[..8].copy_from_slice(
                                    &entry
                                        .decode_index(
                                            read_word(&bytes[..8]) as u64,
                                            row_index,
                                            frame.len(),
                                        )
                                        .to_ne_bytes(),
                                );
                                raw[8..12].copy_from_slice(
                                    &crate::subscriber::storage::CallsiteArchetype::from_bits(
                                        entry.identity,
                                    )
                                    .callsite()
                                    .to_ne_bytes(),
                                );
                                raw[12..].copy_from_slice(
                                    &crate::subscriber::storage::CallsiteArchetype::from_bits(
                                        entry.identity,
                                    )
                                    .archetype()
                                    .to_ne_bytes(),
                                );
                                read_word(&raw)
                            }
                            _ => read_word(&row[field.range.clone()]),
                        };
                        if row_index == 0 {
                            first[index] = value;
                        }
                        masks[index] |= value ^ first[index];
                    }
                    row_index += 1;
                }
                let mut retained_row_bits = 0;
                let mut raw_row_bits = 0;
                for (index, field) in fields.iter().enumerate() {
                    let record = field.record.map_or(&[][..], |index| records[index]);
                    let encoding = if field.alias.is_some() {
                        "alias"
                    } else if field.name == "start_ts" {
                        if SplitDictionaryEncoding::is_tagged(record) {
                            if block.data.timestamp_parent_relative(layout) { "parent_offset+split_dictionary" } else { "split_dictionary" }
                        } else if block.data.timestamp_parent_relative(layout) {
                            "parent_offset"
                        } else if record.is_empty()
                            || u16::from_le_bytes(record[8..10].try_into().unwrap()) == 0
                        {
                            "offset"
                        } else {
                            "offset+stride"
                        }
                    } else if is_reference(&field.name) {
                        if (0..parents.len()).any(|g| parents.entry(g as u16).stride.to_u64() != 0)
                        {
                            "identity_dictionary+offset+stride"
                        } else {
                            "identity_dictionary+offset"
                        }
                    } else {
                        encoding(record)
                    };
                    // Projection decodes ZigZag for the logical baselines above;
                    // label the physical transform even when no codec was fitted.
                    let zigzag = projection.is_some_and(|p| {
                        p.fields
                            .iter()
                            .any(|f| f.logical.range() == field.range && f.signed)
                    });
                    let encoding = if zigzag && field.alias.is_none() {
                        if encoding == "raw" {
                            "zigzag".to_owned()
                        } else {
                            format!("zigzag+{encoding}")
                        }
                    } else {
                        encoding.to_owned()
                    };
                    let identity = [
                        run.to_owned(),
                        callsite.to_string(),
                        arch.id.to_string(),
                        block.descriptor.index.to_string(),
                        field.name.clone(),
                        field.role.into(),
                        field.primitive.clone(),
                        encoding.into(),
                    ];
                    let source = [
                        site.metadata.target().to_owned(),
                        site.metadata.file().unwrap_or("").to_owned(),
                        site.metadata
                            .line()
                            .map_or(String::new(), |n| n.to_string()),
                        site.metadata.name().to_owned(),
                    ];
                    let mut emit = |component: &str,
                                    entry: Option<usize>,
                                    values: usize,
                                    retained: usize,
                                    raw: usize,
                                    unpacked: usize|
                     -> io::Result<()> {
                        let cells = [
                            component.to_owned(),
                            entry.map_or(String::new(), |n| n.to_string()),
                            values.to_string(),
                            retained.to_string(),
                            raw.to_string(),
                            unpacked.to_string(),
                        ];
                        csv(
                            &mut output,
                            identity.iter().chain(cells.iter()).chain(source.iter()),
                        )
                    };
                    let retained_range = field.stored.clone().unwrap_or(0..0);
                    let retained_width = retained_range.len();
                    let retained_bits: usize = frame.mask()[retained_range]
                        .iter()
                        .map(|byte| {
                            if cfg!(feature = "bit-compression") {
                                byte.count_ones() as usize
                            } else {
                                usize::from(*byte != 0) * 8
                            }
                        })
                        .sum();
                    let raw_bits = masks[index].count_ones() as usize;
                    retained_row_bits += retained_bits;
                    raw_row_bits += raw_bits;
                    emit(
                        "first",
                        None,
                        1,
                        retained_width * 8,
                        field.raw_width * 8,
                        field.raw_width * 8,
                    )?;
                    if let Some(source) = field.alias {
                        emit("alias_source", Some(source), 0, 0, 0, 0)?;
                    }
                    emit("mask", None, 0, retained_width * 8, field.raw_width * 8, 0)?;
                    emit(
                        "varying_rows",
                        None,
                        frame.len() - 1,
                        (frame.len() - 1) * retained_bits,
                        (frame.len() - 1) * raw_bits,
                        (frame.len() - 1) * field.raw_width * 8,
                    )?;
                    if field.record.is_some() {
                        emit("metadata_width", None, 0, 8, 0, 0)?;
                    }
                    if field.name == "start_ts" && !record.is_empty() {
                        // Origin and representation share one word; relative
                        // rows retain that word without a prediction stride.
                        emit("exact_offset", None, 0, 60, 0, 0)?;
                        emit("encoding_tag", None, 0, 4, 0, 0)?;
                        if SplitDictionaryEncoding::is_tagged(record) {
                            // All source/key/table fields and record padding
                            // belong to the new codec, beyond the shared origin.
                            emit("split_dictionary_metadata", None, 0, (record.len() - 8) * 8, 0, 0)?;
                        } else if !block.data.timestamp_parent_relative(layout) {
                            emit("float_stride", None, 0, 16, 0, 0)?;
                        }
                    }
                    // Identity entries are shared by reference columns. Charge
                    // them once to the first reference in row order.
                    if is_reference(&field.name)
                        && !fields[..index]
                            .iter()
                            .any(|field| is_reference(&field.name))
                    {
                        for group in 0..parents.len() {
                            emit("parent_identity", Some(group), 0, 32, 0, 0)?;
                            emit("float_offset", Some(group), 0, 16, 0, 0)?;
                            emit("float_stride", Some(group), 0, 16, 0, 0)?;
                        }
                    }
                    if let Some(&tag) = record.first().filter(|_| field.name != "start_ts") {
                        let payload = record_payload_bytes(record, retained_width, frame.len());
                        emit(
                            "record_padding",
                            None,
                            0,
                            (record.len() - payload) * 8,
                            0,
                            0,
                        )?;
                        let record = &record[..payload];
                        emit("encoding_tag", None, 0, 8, 0, 0)?;
                        match tag {
                            exceptions::TAG => {
                                // The tag was charged above; remaining header
                                // flags/count/width occupy exactly three bytes.
                                emit("exception_header", None, 0, 24, 0, 0)?;
                                emit("exact_offset", None, 0, retained_width * 8, 0, 0)?;
                                emit(
                                    "exception_tables",
                                    None,
                                    0,
                                    (record.len() - 4 - retained_width) * 8,
                                    0,
                                    0,
                                )?;
                            }
                            split_dictionary::TAG => {
                                emit("split_dictionary_metadata", None, 0, (record.len() - 1) * 8, 0, 0)?;
                            }
                            DICTIONARY | DICTIONARY_CYCLE | DICTIONARY_STRIDE => {
                                let count = usize::from(record[1]);
                                let bits = usize::from(record[2]);
                                emit("dictionary_count", None, 0, 8, 0, 0)?;
                                emit("dictionary_entry_width", None, 0, 8, 0, 0)?;
                                for entry in 0..count {
                                    emit("dictionary_entry", Some(entry), 0, bits, 0, 0)?;
                                }
                                emit(
                                    "dictionary_padding",
                                    None,
                                    0,
                                    (count * bits).div_ceil(8) * 8 - count * bits,
                                    0,
                                    0,
                                )?;
                                if tag == DICTIONARY_STRIDE {
                                    emit("float_stride", None, 0, 16, 0, 0)?;
                                }
                                if tag == DICTIONARY_CYCLE {
                                    emit("cycle_period", None, 0, 8, 0, 0)?;
                                    emit("cycle_phase", None, 0, 8, 0, 0)?;
                                }
                            }
                            _ => {
                                emit("exact_offset", None, 0, retained_width * 8, 0, 0)?;
                                if matches!(tag & 0x7f, LINEAR | CYCLE) {
                                    emit("float_stride", None, 0, 16, 0, 0)?;
                                }
                                if matches!(tag & 0x7f, CYCLE | ROW_CYCLE) {
                                    emit("cycle_period", None, 0, 8, 0, 0)?;
                                    emit("cycle_phase", None, 0, 8, 0, 0)?;
                                }
                            }
                        }
                    }
                }
                // Shared row-tail padding is a separate leaf, never attributed arbitrarily to fields.
                let row_count = frame.len() - 1;
                let retained_stride = if cfg!(feature = "bit-compression")
                    && crate::data::dataframe::CONTINUOUS_BIT_PACKING
                {
                    retained_row_bits
                } else {
                    retained_row_bits.div_ceil(8) * 8
                };
                let raw_stride = if crate::data::dataframe::CONTINUOUS_BIT_PACKING {
                    raw_row_bits
                } else {
                    raw_row_bits.div_ceil(8) * 8
                };
                let identity = [
                    run.to_owned(),
                    callsite.to_string(),
                    arch.id.to_string(),
                    block.descriptor.index.to_string(),
                    "@block".into(),
                    "packing".into(),
                    String::new(),
                    "shared".into(),
                    "row_padding".into(),
                    String::new(),
                    "0".into(),
                    ((row_count * retained_stride).div_ceil(8) * 8 - row_count * retained_row_bits)
                        .to_string(),
                    ((row_count * raw_stride).div_ceil(8) * 8 - row_count * raw_row_bits)
                        .to_string(),
                    "0".into(),
                    site.metadata.target().into(),
                    site.metadata.file().unwrap_or("").into(),
                    site.metadata
                        .line()
                        .map_or(String::new(), |n| n.to_string()),
                    site.metadata.name().into(),
                ];
                csv(&mut output, identity.iter())?;
            }
        }
    }
    output.flush()
}

fn encoding(record: &[u8]) -> &'static str {
    match record.first().map(|tag| tag & 0x7f) {
        None => "raw",
        Some(OFFSET) => "offset",
        Some(LINEAR) => "offset+stride",
        Some(ROW) => "offset+row",
        Some(CYCLE) => "offset+stride+cycle",
        Some(ROW_CYCLE) => "offset+row+cycle",
        Some(DICTIONARY) => "dictionary",
        Some(DICTIONARY_CYCLE) => "dictionary+cycle",
        Some(DICTIONARY_STRIDE) => "dictionary+stride",
        Some(exceptions::TAG) => "offset+exceptions",
        Some(split_dictionary::TAG) => "split_dictionary",
        _ => unreachable!("validated codec"),
    }
}

/// Codec sections already round their bit arrays to bytes. The size directory
/// additionally rounds the complete record to two bytes; account for that byte
/// separately rather than charging it to a coefficient or exception table.
fn record_payload_bytes(record: &[u8], width: usize, rows: usize) -> usize {
    let tag = record[0] & 127;
    match tag {
        split_dictionary::TAG => SplitDictionaryEncoding::parse(record, Layout::new(0, width, false), rows).payload_bytes(),
        DICTIONARY | DICTIONARY_CYCLE | DICTIONARY_STRIDE => {
            let header = if tag == DICTIONARY { 3 } else { 5 };
            header + (usize::from(record[1]) * usize::from(record[2])).div_ceil(8)
        }
        exceptions::TAG => {
            let split = usize::from(record[1] & 127);
            let count = usize::from(record[2]);
            let positions = if record[1] & 128 != 0 {
                rows
            } else {
                count * (usize::BITS - (rows - 1).leading_zeros()) as usize
            };
            4 + width
                + if record[3] & 128 != 0 {
                    (width * 8 - split).div_ceil(8)
                } else {
                    0
                }
                + positions.div_ceil(8)
                + (count * usize::from(record[3] & 127)).div_ceil(8)
        }
        _ => {
            1 + width
                + if matches!(tag, LINEAR | CYCLE) { 2 } else { 0 }
                + if matches!(tag, CYCLE | ROW_CYCLE) {
                    2
                } else {
                    0
                }
        }
    }
}

fn csv<'a>(output: &mut impl Write, cells: impl Iterator<Item = &'a String>) -> io::Result<()> {
    for (index, cell) in cells.enumerate() {
        if index != 0 {
            write!(output, ",")?;
        }
        write!(output, "\"{}\"", cell.replace('"', "\"\""))?;
    }
    writeln!(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscriber::LocalRecordingLayer;
    use std::sync::Arc;
    use tracing_subscriber::{Registry, layer::SubscriberExt};

    #[test]
    fn component_leaves_reconcile_and_inspection_does_not_change_storage() {
        let recorder = Arc::new(RecordingLayer::new());
        tracing::subscriber::with_default(
            Registry::default().with(LocalRecordingLayer(recorder.clone())),
            || {
                let predecessor = tracing::info_span!("metric_predecessor");
                for row in 0..512u64 {
                    let span = tracing::info_span!(
                        "metric_span",
                        sequence = row,
                        later = tracing::field::Empty
                    );
                    // Include two reference columns and their shared dictionary
                    // in the independent byte reconciliation below.
                    span.follows_from(predecessor.id());
                    span.record("later", row);
                    let _entered = span.enter();
                    // Aliases keep logical value counts but must not repeat
                    // physical bytes in the reconciliation below.
                    tracing::info!(
                        value = row % 17,
                        copy = row % 17,
                        flag = row % 2 == 0,
                        // Force an exception-coded physical field so its exact
                        // four-byte header/tables participate in reconciliation.
                        sparse = (1u64 << 40) + if row % 127 == 0 { 1 << 30 } else { row % 31 },
                        signed = if row % 2 == 0 { 1i64 } else { -1i64 }
                    );
                }
            },
        );
        let snapshot = || {
            let mut expected = 0;
            let mut trailers = Vec::new();
            for shared in recorder.callsites.read().values() {
                for arch in &shared.read().cache {
                    for block in &arch.data.ring {
                        let frame = &block.data.frame;
                        expected += (frame.compression_ratio().compressed
                            + frame.unit()
                            + frame.trailer().len())
                            * 8;
                        // Retention age is domain metadata rather than a field codec.
                        let retained = block
                            .data
                            .retention_footer_bytes(&arch.archetype.column_layout());
                        expected -= retained * 8;
                        trailers.push(frame.trailer().to_vec());
                    }
                }
            }
            trailers.sort();
            (expected, trailers)
        };
        let before = snapshot();
        let mut output = Vec::new();
        write_recording(&mut output, "unit", &recorder, None).unwrap();
        assert_eq!(snapshot(), before);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("\"alias\""));
        assert!(text.contains("offset+exceptions"));
        let mut retained = 0;
        let mut signed_seen = false;
        for row in text.lines().skip(1) {
            // Fixture source labels intentionally contain no commas or line breaks.
            let cells: Vec<_> = row.split(',').map(|cell| cell.trim_matches('"')).collect();
            assert_eq!(cells.len(), 18);
            assert!(!cells[1].is_empty(), "every leaf retains its callsite");
            assert_ne!(cells[8], "total");
            if cells[4] == "field.signed" && cells[8] == "varying_rows" {
                // Keep the baseline in logical two's-complement form even
                // though retained columns use ZigZag before any fitted codec.
                assert!(cells[7].starts_with("zigzag"));
                assert_eq!(
                    cells[12].parse::<usize>().unwrap(),
                    cells[10].parse::<usize>().unwrap() * 63
                );
                signed_seen = true;
            }
            retained += cells[11].parse::<usize>().unwrap();
        }
        assert!(signed_seen);
        assert_eq!(retained, before.0);
    }
}
