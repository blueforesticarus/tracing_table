//! Inspect dumped messages with template strategies and comparison variants.
//! Each row prints: algorithm tag, archetype, target, template, then values for {} slots.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::time::{Duration, Instant};

#[path = "learned.rs"]
mod learned;
use learned::LearnedState;
mod drain2;
mod preextract;

// Change this ANSI SGR code to tune the template color for a terminal theme.
const TEMPLATE_COLOR: &str = "\x1b[1;36m";
const COLOR_RESET: &str = "\x1b[0m";
// Tune these RGB endpoints to suit the terminal theme. Each numeric report
// column interpolates independently from blue (lower cost) to red (higher cost).
const REPORT_LOW_RGB: [u8; 3] = [80, 165, 255];
const REPORT_HIGH_RGB: [u8; 3] = [235, 75, 75];
const REPORT_ZERO_RGB: [u8; 3] = [90, 90, 90];
// Keep the original seven tag positions stable; the new comparison branches
// use the final M and D positions.
const TAG_BYTES: &[u8; 9] = b"aqmdspnMD";

#[derive(Clone, Copy)]
struct Token {
    start: usize,
    end: usize,
}

#[derive(Clone)]
struct Row {
    archetype: String,
    target: String,
    message: String,
    tokens: Vec<Token>,
    // Typed regex spans are atomic tokens; empty for an unmasked input row.
    masked: Vec<Option<ValueType>>,
}

impl Row {
    fn token(&self, index: usize) -> &str {
        if let Some(Some(kind)) = self.masked.get(index) {
            // These multi-character markers cannot collide with a literal
            // token from the punctuation tokenizer. They are never printed.
            return match kind {
                ValueType::Number => "<number>",
                ValueType::Ip => "<ip>",
                ValueType::Path => "<path>",
                ValueType::Module => "<module>",
                ValueType::String => "<string>",
            };
        }
        let token = self.tokens[index];
        &self.message[token.start..token.end]
    }

    fn inside_mask(&self, offset: usize) -> bool {
        // A cached generic template must not split a preextracted regex span.
        if self.masked.is_empty() {
            return false;
        }
        let index = self.tokens.partition_point(|token| token.end <= offset);
        self.tokens
            .get(index)
            .is_some_and(|token| token.start < offset)
            && self.masked.get(index).is_some_and(Option::is_some)
    }

    fn contains_mask(&self, start: usize, end: usize) -> bool {
        if self.masked.is_empty() {
            return false;
        }
        let index = self.tokens.partition_point(|token| token.end <= start);
        self.tokens[index..]
            .iter()
            .zip(self.masked.iter().skip(index))
            .take_while(|(token, _)| token.start < end)
            .any(|(_, kind)| kind.is_some())
    }
}

#[derive(Clone, Copy)]
enum Algorithm {
    Msa,
    Sequitur,
    Mdl,
    Drain,
    Spell,
    Prefix,
    None,
    Mdl2,
    Drain2,
}

impl Algorithm {
    const ALL: [Self; 9] = [
        Self::Msa,
        Self::Sequitur,
        Self::Mdl,
        Self::Mdl2,
        Self::Drain,
        Self::Drain2,
        Self::Spell,
        Self::Prefix,
        Self::None,
    ];

    fn index(self) -> usize {
        match self {
            Self::Msa => 0,
            Self::Sequitur => 1,
            Self::Mdl => 2,
            Self::Drain => 3,
            Self::Spell => 4,
            Self::Prefix => 5,
            Self::None => 6,
            Self::Mdl2 => 7,
            Self::Drain2 => 8,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "msa" => Some(Self::Msa),
            "sequitur" | "seq" => Some(Self::Sequitur),
            "mdl" => Some(Self::Mdl),
            "mdl2" => Some(Self::Mdl2),
            "drain" | "drain3" => Some(Self::Drain),
            "drain2" => Some(Self::Drain2),
            "spell" => Some(Self::Spell),
            "prefix" => Some(Self::Prefix),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Msa => "msa",
            Self::Sequitur => "seq",
            Self::Mdl => "mdl",
            Self::Drain => "drain",
            Self::Spell => "spell",
            Self::Prefix => "prefix",
            Self::None => "none",
            Self::Mdl2 => "mdl2",
            Self::Drain2 => "drain2",
        }
    }
}

#[derive(Default)]
struct Timing {
    elapsed: Duration,
    // Codec-only work on block-local pools; separate from algorithm timing.
    compression_elapsed: Duration,
    weighted_ms: f64,
    // Dictionary construction is separate from extraction and codec work.
    // Weight partial blocks by rows, using the same divisor as avg/blk.
    dedup_weighted_ms: f64,
    blocks: u64,
    rows: u64,
    full_blocks: u64,
    distinct_patterns: u64,
    distinct_archetypes: u64,
    sizes: StringSizes,
}

impl Timing {
    fn record_block(&mut self, elapsed: Duration, rows: usize) {
        // A partial block contributes in proportion to its message count.
        self.elapsed += elapsed;
        self.weighted_ms += elapsed.as_secs_f64() * 1000.0 * rows as f64;
        self.blocks += 1;
        self.rows += rows as u64;
    }

    fn weighted_avg_ms_per_block(&self) -> Option<f64> {
        (self.rows > 0).then(|| self.weighted_ms / self.rows as f64)
    }

    fn record_dedup_block(&mut self, elapsed: Duration, rows: usize) {
        self.dedup_weighted_ms += elapsed.as_secs_f64() * 1000.0 * rows as f64;
    }

    fn weighted_avg_dedup_ms_per_block(&self) -> Option<f64> {
        (self.rows > 0).then(|| self.dedup_weighted_ms / self.rows as f64)
    }

    fn avg_compression_ms_per_full_block(&self) -> Option<f64> {
        (self.full_blocks > 0)
            .then(|| self.compression_elapsed.as_secs_f64() * 1000.0 / self.full_blocks as f64)
    }

    fn record_full_block(&mut self, summary: &BlockSummary) {
        // Only complete blocks contribute to these unweighted block averages.
        self.full_blocks += 1;
        self.distinct_patterns += summary.patterns.len() as u64;
        self.distinct_archetypes += summary.archetypes.len() as u64;
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ValueType {
    String = 0,
    Number = 1,
    Ip = 2,
    Path = 3,
    Module = 4,
}

impl ValueType {
    fn label(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Ip => "ip",
            Self::Path => "path",
            Self::Module => "module",
        }
    }
}

fn value_type(value: &str) -> ValueType {
    // A small, shared field classifier for report statistics only. It never
    // changes extraction, template text, or the bytes stored as a value.
    let value = value.trim().trim_matches(['"', '\'']);
    if value
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .is_ok()
    {
        return ValueType::Ip;
    }
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"));
    let is_number = hex
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()))
        || (value.bytes().any(|b| b.is_ascii_digit())
            && value.parse::<f64>().is_ok_and(f64::is_finite));
    if is_number {
        return ValueType::Number;
    }
    if !value.contains(char::is_whitespace) && (value.contains('/') || value.contains('\\')) {
        return ValueType::Path;
    }
    if value.contains("::")
        && value.split("::").all(|part| {
            !part.is_empty() && part.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
        })
    {
        return ValueType::Module;
    }
    ValueType::String
}

fn value_archetype<'a>(values: impl IntoIterator<Item = &'a str>, typed: bool) -> [u32; 5] {
    // The counts retain multiplicity while making field order irrelevant:
    // [string, ip] and [ip, string] match, but two strings do not.
    let mut counts = [0; 5];
    for value in values {
        let kind = if typed {
            value_type(value)
        } else {
            ValueType::String
        };
        counts[kind as usize] += 1;
    }
    counts
}

#[derive(Default)]
struct BlockSummary {
    patterns: HashSet<String>,
    archetypes: HashSet<[u32; 5]>,
}

impl BlockSummary {
    fn record_types(&mut self, template: Option<&str>, types: &[ValueType]) {
        // Share the actual capture schema with the printed placeholders;
        // whole-message baselines contribute a schema but no stored pattern.
        if let Some(template) = template {
            self.patterns.insert(template.to_owned());
        }
        let mut counts = [0; 5];
        for kind in types {
            counts[*kind as usize] += 1;
        }
        self.archetypes.insert(counts);
    }

    fn record<'a>(
        &mut self,
        template: Option<&str>,
        values: impl IntoIterator<Item = &'a str>,
        typed: bool,
    ) {
        if let Some(template) = template {
            self.patterns.insert(template.to_owned());
        }
        self.archetypes.insert(value_archetype(values, typed));
    }
}

#[derive(Default)]
struct StringSizes {
    templates: HashSet<String>,
    values: HashSet<String>,
    unique_template_bytes: u64,
    unique_value_bytes: u64,
    value_bytes: u64,
    value_count: u64,
}

impl StringSizes {
    fn record(&mut self, template: &str, fields: &[String]) {
        // Deduplicate across the entire input, separately for each algorithm
        // and for templates versus individual value strings. Ignore TSV/ANSI
        // formatting; template byte counts include the {} slot markers.
        if self.templates.insert(template.to_owned()) {
            self.unique_template_bytes += unescape(template).len() as u64;
        }
        for value in fields {
            self.record_value(value);
        }
    }

    fn record_value(&mut self, value: &str) {
        // Failed templates store the original message in the same value pool
        // as extracted fields, so identical strings deduplicate across both.
        let bytes = value.len() as u64;
        self.value_count += 1;
        self.value_bytes += bytes;
        if self.values.insert(value.to_owned()) {
            self.unique_value_bytes += bytes;
        }
    }

    fn unique_payload(&self) -> Vec<u8> {
        // Compress the same raw bytes counted in total, in a stable order.
        // Templates and values retain separate pools; lengths, row references,
        // and framing are excluded just like the other storage estimates.
        let mut templates: Vec<_> = self.templates.iter().collect();
        let mut values: Vec<_> = self.values.iter().collect();
        templates.sort_unstable();
        values.sort_unstable();
        let mut payload = Vec::new();
        for template in templates {
            payload.extend_from_slice(unescape(template).as_bytes());
        }
        for value in values {
            payload.extend_from_slice(value.as_bytes());
        }
        payload
    }

    fn lz4_bytes(&self) -> u64 {
        let payload = self.unique_payload();
        if payload.is_empty() {
            0
        } else {
            lz4_flex::block::compress(&payload).len() as u64
        }
    }
}

fn format_bytes_per_row(bytes: u64, rows: u64) -> String {
    // The caller supplies rows from archetypes with at least two messages.
    // Failed templates still count as rows; singleton values remain in bytes.
    if rows == 0 {
        return "n/a".to_owned();
    }
    if bytes == 0 {
        return "0".to_owned();
    }
    let units = ["", "K", "M", "G"];
    let mut value = bytes as f64 / rows as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < units.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let factor = 10f64.powi(2 - value.log10().floor() as i32);
    value = (value * factor).round() / factor;
    if value >= 1000.0 && unit < units.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let decimals = (2 - value.log10().floor() as i32).max(0) as usize;
    format!("{value:.decimals$}{}", units[unit])
}

fn format_ms(ms: f64) -> String {
    // Use decimal notation with three significant figures where possible;
    // preserve every integer digit when a measurement exceeds that precision.
    if ms == 0.0 {
        return "0".to_owned();
    }
    let decimals = (2 - ms.log10().floor() as i32).max(0) as usize;
    format!("{ms:.decimals$}")
}

fn align_decimal_places(rows: &mut [Vec<ReportCell>], columns: &[usize]) {
    // Keep enough precision for the smallest timings, then pad larger values
    // to the same decimal count. Missing measurements remain unformatted.
    for &column in columns {
        let precision = rows
            .iter()
            .map(|row| {
                row[column]
                    .text
                    .split_once('.')
                    .map_or(0, |(_, fraction)| fraction.len())
            })
            .max()
            .unwrap_or(0);
        for row in rows.iter_mut() {
            if let Some(value) = row[column].numeric {
                row[column].text = format!("{value:.precision$}");
            }
        }
    }
}

struct ReportCell {
    text: String,
    numeric: Option<f64>,
}

impl ReportCell {
    fn label(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            numeric: None,
        }
    }

    fn number(text: impl Into<String>, value: f64) -> Self {
        Self {
            text: text.into(),
            numeric: Some(value),
        }
    }
}

fn byte_cell(bytes: u64, rows: u64) -> ReportCell {
    let text = format_bytes_per_row(bytes, rows);
    ReportCell {
        text,
        numeric: (rows > 0).then(|| bytes as f64 / rows as f64),
    }
}

fn report_rgb(value: f64, low: f64, high: f64) -> [u8; 3] {
    // Zero measurements use a neutral dark grey instead of the gradient.
    if value == 0.0 {
        return REPORT_ZERO_RGB;
    }
    // Equal columns use the gradient midpoint, rather than implying a minimum
    // or maximum where every algorithm has the same measurement.
    let fraction = if high > low {
        ((value - low) / (high - low)).clamp(0.0, 1.0)
    } else {
        0.5
    };
    std::array::from_fn(|i| {
        (REPORT_LOW_RGB[i] as f64
            + fraction * (REPORT_HIGH_RGB[i] as f64 - REPORT_LOW_RGB[i] as f64))
            .round() as u8
    })
}

fn write_colored_number(report: &mut impl Write, text: &str, rgb: [u8; 3]) -> io::Result<()> {
    // Decimal points do not interrupt leading zero digits. Every zero after
    // the first nonzero digit retains the column color.
    let nonzero_digits: Vec<_> = text
        .char_indices()
        .filter(|(_, ch)| ch.is_ascii_digit() && *ch != '0')
        .map(|(index, _)| index)
        .collect();
    let first_nonzero = nonzero_digits.first();
    let mut active = None;
    for (index, ch) in text.char_indices() {
        let edge_zero = ch == '0' && first_nonzero.is_none_or(|&first| index < first);
        let color = if edge_zero { REPORT_ZERO_RGB } else { rgb };
        if active != Some(color) {
            let [r, g, b] = color;
            write!(report, "\x1b[38;2;{r};{g};{b}m")?;
            active = Some(color);
        }
        write!(report, "{ch}")?;
    }
    write!(report, "{COLOR_RESET}")
}

fn write_aligned_table(
    report: &mut impl Write,
    headers: &[&str],
    rows: &[Vec<ReportCell>],
) -> io::Result<()> {
    // Compare unrounded values, so display rounding and K/M/G suffixes do not
    // affect the gradient. ANSI codes stay outside the space padding.
    // Align at the actual or implied decimal point instead of the final
    // character, since fractional precision and unit suffixes vary by row.
    let decimal_anchor = |text: &str| {
        text.find('.').unwrap_or_else(|| {
            text.find(|ch: char| ch.is_ascii_alphabetic())
                .unwrap_or(text.len())
        })
    };
    let decimal_widths: Vec<_> = (0..headers.len())
        .map(|index| {
            rows.iter().filter(|row| row[index].numeric.is_some()).fold(
                (0, 0),
                |(left, right), row| {
                    let text = &row[index].text;
                    let anchor = decimal_anchor(text);
                    (left.max(anchor), right.max(text.len() - anchor))
                },
            )
        })
        .collect();
    let widths: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| row[index].text.len())
                .max()
                .unwrap_or(0)
                .max(header.len())
                .max(decimal_widths[index].0 + decimal_widths[index].1)
        })
        .collect();
    let bounds: Vec<_> = (0..headers.len())
        .map(|index| {
            let values: Vec<_> = rows
                .iter()
                .filter_map(|row| row[index].numeric)
                .filter(|value| value.is_finite())
                .collect();
            if values.is_empty() {
                None
            } else {
                Some((
                    values.iter().copied().fold(f64::INFINITY, f64::min),
                    values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                ))
            }
        })
        .collect();
    for (index, header) in headers.iter().enumerate() {
        if index > 0 {
            write!(report, "  ")?;
        }
        if index == 0 {
            write!(report, "{header:<width$}", width = widths[index])?;
        } else {
            write!(report, "{header:>width$}", width = widths[index])?;
        }
    }
    writeln!(report)?;
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                write!(report, "  ")?;
            }
            if index == 0 {
                write!(report, "{:<width$}", cell.text, width = widths[index])?;
            } else {
                let (_, right) = decimal_widths[index];
                let anchor = decimal_anchor(&cell.text);
                let (before, after) = if cell.numeric.is_some() {
                    (
                        widths[index] - right - anchor,
                        right - (cell.text.len() - anchor),
                    )
                } else {
                    (widths[index] - cell.text.len(), 0)
                };
                write!(report, "{:>padding$}", "", padding = before)?;
                if let (Some(value), Some((low, high))) = (cell.numeric, bounds[index]) {
                    write_colored_number(report, &cell.text, report_rgb(value, low, high))?;
                } else {
                    write!(report, "{}", cell.text)?;
                }
                write!(report, "{:after$}", "")?;
            }
        }
        writeln!(report)?;
    }
    Ok(())
}

fn print_timings(selected: &[bool; 9], timings: &[Timing; 9], preextract: bool) -> io::Result<()> {
    // Render once so the colocated artifact and terminal receive identical bytes,
    // including headings, alignment, and ANSI colors, regardless of caller cwd.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/dump_messsage_compression/formats.result");
    let mut report = Vec::new();
    writeln!(
        report,
        "Shared regex preextraction: {}; tokenization{} included in algorithm timing.",
        if preextract { "on" } else { "off" },
        if preextract {
            " and regex preprocessing"
        } else {
            ""
        }
    )?;
    writeln!(
        report,
        "Timing (ms; avg/blk and dedup/blk row-weighted; lz4/blk full blocks only):"
    )?;
    let mut time_rows = Vec::new();
    for algorithm in Algorithm::ALL {
        let index = algorithm.index();
        if !selected[index] {
            continue;
        }
        let timing = &timings[index];
        let total_ms = timing.elapsed.as_secs_f64() * 1000.0;
        let average = timing.weighted_avg_ms_per_block().map_or_else(
            || ReportCell::label("n/a"),
            |ms| ReportCell::number(format_ms(ms), ms),
        );
        let compression = timing.avg_compression_ms_per_full_block().map_or_else(
            || ReportCell::label("n/a"),
            |ms| ReportCell::number(format_ms(ms), ms),
        );
        let dedup = timing.weighted_avg_dedup_ms_per_block().map_or_else(
            || ReportCell::label("n/a"),
            |ms| ReportCell::number(format_ms(ms), ms),
        );
        time_rows.push(vec![
            ReportCell::label(algorithm.name()),
            ReportCell::number(format_ms(total_ms), total_ms),
            average,
            compression,
            dedup,
        ]);
    }
    align_decimal_places(&mut time_rows, &[1, 2, 3, 4]);
    write_aligned_table(
        &mut report,
        &["algo", "total", "avg/blk", "lz4/blk", "dedup/blk"],
        &time_rows,
    )?;
    writeln!(
        report,
        "String sizes and block diversity (UTF-8 B/non-singleton row; tpl/val deduped; all_val retains duplicates; singleton values included; K/M/G decimal; 3 sig figs):"
    )?;
    let complete_blocks = Algorithm::ALL
        .into_iter()
        .find(|algorithm| selected[algorithm.index()])
        .map_or(0, |algorithm| timings[algorithm.index()].full_blocks);
    writeln!(
        report,
        "Block diversity averages use {complete_blocks} complete blocks; short blocks are excluded."
    )?;
    let mut size_rows = Vec::new();
    for algorithm in Algorithm::ALL {
        let index = algorithm.index();
        if selected[index] {
            let timing = &timings[index];
            let sizes = &timing.sizes;
            let average = |sum: u64| {
                if timing.full_blocks == 0 {
                    ReportCell::label("n/a")
                } else {
                    let value = sum as f64 / timing.full_blocks as f64;
                    ReportCell::number(format!("{value:.2}"), value)
                }
            };
            size_rows.push(vec![
                ReportCell::label(algorithm.name()),
                byte_cell(sizes.unique_template_bytes, timings[index].rows),
                byte_cell(sizes.unique_value_bytes, timings[index].rows),
                byte_cell(sizes.value_bytes, timings[index].rows),
                byte_cell(
                    sizes.unique_template_bytes + sizes.unique_value_bytes,
                    timings[index].rows,
                ),
                byte_cell(sizes.lz4_bytes(), timings[index].rows),
                ReportCell::number(
                    sizes.templates.len().to_string(),
                    sizes.templates.len() as f64,
                ),
                ReportCell::number(sizes.value_count.to_string(), sizes.value_count as f64),
                average(timing.distinct_patterns),
                average(timing.distinct_archetypes),
            ]);
        }
    }
    let headers = [
        "algo", "tpl", "val", "all_val", "total", "lz4", "tpls", "vals", "pat/blk", "arch/blk",
    ];
    write_aligned_table(&mut report, &headers, &size_rows)?;
    std::fs::write(path, &report)?;
    // Keep the summary on stderr so stdout remains a stream of template records.
    io::stderr().lock().write_all(&report)
}

fn tokenize(message: &str) -> Vec<Token> {
    // Keep punctuation as individual tokens so numbers inside brackets can vary.
    let mut tokens = Vec::new();
    let mut word_start = None;
    for (index, ch) in message.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            word_start.get_or_insert(index);
        } else {
            if let Some(start) = word_start.take() {
                tokens.push(Token { start, end: index });
            }
            tokens.push(Token {
                start: index,
                end: index + ch.len_utf8(),
            });
        }
    }
    if let Some(start) = word_start {
        tokens.push(Token {
            start,
            end: message.len(),
        });
    }
    tokens
}

fn unescape(value: &str) -> String {
    // Match the four escapes written by --dump_strings, including escaped backslashes.
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('t') => result.push('\t'),
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some(other) => result.push(other),
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn escape_template_literal(value: &str) -> String {
    // Reserve bare {} for variable slots even when a log contains literal braces.
    escape(value).replace('{', "\\{").replace('}', "\\}")
}

fn lcs_indices<T: PartialEq>(left: &[T], right: &[T]) -> Vec<usize> {
    // Keep exact matches cheap. Otherwise, skip oversized pairwise alignment
    // instead of substituting another method or blocking on a giant message.
    if left == right {
        return (0..left.len()).collect();
    }
    if left.len().saturating_mul(right.len()) > 16_384 {
        return Vec::new();
    }
    let width = right.len() + 1;
    let mut lengths = vec![0u16; (left.len() + 1) * width];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            lengths[i * width + j] = if left[i] == right[j] {
                1 + lengths[(i + 1) * width + j + 1]
            } else {
                lengths[(i + 1) * width + j].max(lengths[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut kept = Vec::new();
    while i < left.len() && j < right.len() {
        if left[i] == right[j] {
            kept.push(i);
            i += 1;
            j += 1;
        } else if lengths[(i + 1) * width + j] >= lengths[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    kept
}

fn row_words(row: &Row) -> Vec<String> {
    row.tokens
        .iter()
        .map(|t| row.message[t.start..t.end].to_owned())
        .collect()
}

fn aligned_candidates(rows: &[&Row]) -> Vec<usize> {
    let first = rows[0];
    // This is strictly positional. MSA has its own alignment path; MDL,
    // Drain, and Sequitur do not switch methods when token counts differ.
    let count = rows.iter().map(|row| row.tokens.len()).min().unwrap_or(0);
    (0..count)
        .filter(|&i| rows.iter().all(|row| row.token(i) == first.token(i)))
        .collect()
}

fn msa_candidates(rows: &[&Row]) -> Vec<usize> {
    let first = rows[0];
    let mut candidates: Vec<usize> = (0..first.tokens.len()).collect();
    // A small progressive alignment finds internal anchors; the later linear
    // validation pass checks every row, including rows beyond this sample.
    for row in rows.iter().skip(1).take(31) {
        // Borrow token slices from already-parsed rows; the old path cloned
        // every token for each of the sampled alignments.
        let left: Vec<_> = candidates.iter().map(|&i| first.token(i)).collect();
        let right: Vec<_> = (0..row.tokens.len()).map(|i| row.token(i)).collect();
        let kept = lcs_indices(&left, &right);
        candidates = kept.into_iter().map(|i| candidates[i]).collect();
        if candidates.is_empty() {
            break;
        }
    }
    candidates
}

fn matching_positions(rows: &[&Row], candidates: &mut Vec<usize>) -> Vec<Vec<usize>> {
    let first = rows[0];
    // Remove any anchor that a later row cannot match in order. This bounds
    // expensive LCS work while ensuring all emitted templates reconstruct rows.
    for row in rows {
        let mut cursor = 0;
        candidates.retain(|&candidate| {
            let wanted = first.token(candidate);
            if let Some(index) = (cursor..row.tokens.len()).find(|&i| row.token(i) == wanted) {
                cursor = index + 1;
                true
            } else {
                false
            }
        });
    }
    rows.iter()
        .map(|row| {
            let mut cursor = 0;
            candidates
                .iter()
                .map(|&candidate| {
                    let index = (cursor..row.tokens.len())
                        .find(|&i| row.token(i) == first.token(candidate))
                        .expect("validated anchor");
                    cursor = index + 1;
                    index
                })
                .collect()
        })
        .collect()
}

fn chosen_anchors(rows: &[&Row], candidates: &[usize], positions: &[Vec<usize>]) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let slot_cost = rows.len() as i64 + 2; // {} in the template and one field per row.
    let mut score = vec![0i64; candidates.len()];
    let mut previous = vec![None; candidates.len()];
    let mut best_previous = None;
    for i in 0..candidates.len() {
        let length = rows[0].token(candidates[i]).len() as i64;
        let reward = (rows.len() as i64 - 1) * length;
        let leading_gap = rows
            .iter()
            .enumerate()
            .any(|(r, row)| row.tokens[positions[r][i]].start > 0);
        let mut base = if leading_gap { -slot_cost } else { 0 };
        let mut from = None;
        if let Some(best) = best_previous {
            if score[best] - slot_cost > base {
                base = score[best] - slot_cost;
                from = Some(best);
            }
        }
        if i > 0
            && rows.iter().enumerate().all(|(r, row)| {
                row.tokens[positions[r][i - 1]].end == row.tokens[positions[r][i]].start
            })
            && score[i - 1] > base
        {
            base = score[i - 1];
            from = Some(i - 1);
        }
        score[i] = base + reward;
        previous[i] = from;
        if best_previous.is_none_or(|best| score[i] > score[best]) {
            best_previous = Some(i);
        }
    }
    let (best, value) = (0..candidates.len())
        .map(|i| {
            let trailing_gap = rows
                .iter()
                .enumerate()
                .any(|(r, row)| row.tokens[positions[r][i]].end < row.message.len());
            (i, score[i] - if trailing_gap { slot_cost } else { 0 })
        })
        .max_by_key(|&(_, value)| value)
        .unwrap();
    if value <= 0 {
        return Vec::new();
    }
    let mut chosen = Vec::new();
    let mut cursor = Some(best);
    while let Some(i) = cursor {
        chosen.push(i);
        cursor = previous[i];
    }
    chosen.reverse();
    chosen
}

fn render_template(
    rows: &[&Row],
    initial_candidates: Vec<usize>,
) -> Option<(String, Vec<Vec<String>>)> {
    // Mask markers can guide alignment/clustering, but cannot become literal
    // anchors. The scorer still uses original byte offsets and residual sizes.
    let mut candidates: Vec<_> = initial_candidates
        .into_iter()
        .filter(|&index| !rows[0].masked.get(index).is_some_and(Option::is_some))
        .collect();
    let positions = matching_positions(rows, &mut candidates);
    let chosen = chosen_anchors(rows, &candidates, &positions);
    // A raw-message {} template is not an algorithm result for this block.
    if chosen.is_empty() {
        return None;
    }
    let mut template = String::new();
    let mut fields = vec![Vec::new(); rows.len()];
    let mut previous_ends = vec![0; rows.len()];
    for &choice in &chosen {
        let starts: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(r, row)| row.tokens[positions[r][choice]].start)
            .collect();
        if rows
            .iter()
            .enumerate()
            .any(|(r, _)| previous_ends[r] < starts[r])
        {
            template.push_str("{}");
            for (r, row) in rows.iter().enumerate() {
                fields[r].push(row.message[previous_ends[r]..starts[r]].to_owned());
            }
        }
        let token = rows[0].token(candidates[choice]);
        template.push_str(&escape_template_literal(token));
        for (r, row) in rows.iter().enumerate() {
            previous_ends[r] = row.tokens[positions[r][choice]].end;
        }
    }
    if rows
        .iter()
        .enumerate()
        .any(|(r, row)| previous_ends[r] < row.message.len())
    {
        template.push_str("{}");
        for (r, row) in rows.iter().enumerate() {
            fields[r].push(row.message[previous_ends[r]..].to_owned());
        }
    }
    Some((template, fields))
}

fn prefix_render(rows: &[Row]) -> Vec<Option<Rendering>> {
    if rows
        .iter()
        .any(|row| row.masked.iter().any(Option::is_some))
    {
        // An optional masked prefix is a sequence of complete literal/typed
        // tokens. It may contain parameter slots before the residual suffix.
        let common = (0..rows[0].tokens.len())
            .take_while(|&index| {
                rows.iter()
                    .all(|row| index < row.tokens.len() && row.token(index) == rows[0].token(index))
            })
            .collect();
        let selected: Vec<_> = rows.iter().collect();
        return render_template(&selected, common).map_or_else(
            || vec![None; rows.len()],
            |(template, fields)| {
                fields
                    .into_iter()
                    .map(|fields| Some((template.clone(), fields)))
                    .collect()
            },
        );
    }
    if rows.len() < 2 {
        return vec![None; rows.len()];
    }
    // Compare characters so the shared prefix ends at a UTF-8 boundary.
    let first = &rows[0].message;
    let mut length = first.len();
    for row in &rows[1..] {
        length = first[..length]
            .chars()
            .zip(row.message.chars())
            .take_while(|(a, b)| a == b)
            .map(|(ch, _)| ch.len_utf8())
            .sum();
    }
    if length == 0 {
        return vec![None; rows.len()];
    }
    let has_suffix = rows.iter().any(|row| row.message.len() > length);
    let slot_cost = if has_suffix { rows.len() + 2 } else { 0 };
    if length * (rows.len() - 1) <= slot_cost {
        return vec![None; rows.len()];
    }
    let mut template = escape_template_literal(&first[..length]);
    if has_suffix {
        template.push_str("{}");
    }
    rows.iter()
        .map(|row| {
            Some((
                template.clone(),
                if has_suffix {
                    vec![row.message[length..].to_owned()]
                } else {
                    Vec::new()
                },
            ))
        })
        .collect()
}

fn drain_clusters(rows: &[Row]) -> Vec<usize> {
    // Drain-like fixed token-count/leading-token buckets with a similarity gate.
    let mut buckets: HashMap<(usize, String), Vec<usize>> = HashMap::new();
    let mut templates: Vec<Vec<Option<String>>> = Vec::new();
    let mut assignments = Vec::with_capacity(rows.len());
    for row in rows {
        let key = (
            row.tokens.len(),
            row.tokens
                .first()
                .map(|_| row.token(0).to_owned())
                .unwrap_or_default(),
        );
        let members = buckets.entry(key).or_default();
        let mut chosen = None;
        let mut best_score = 0;
        for &cluster in members.iter() {
            let score = templates[cluster]
                .iter()
                .enumerate()
                .filter(|(i, token)| token.as_deref() == Some(row.token(*i)))
                .count();
            if score > best_score {
                best_score = score;
                chosen = Some(cluster);
            }
        }
        let cluster = if best_score * 2 >= row.tokens.len().max(1) {
            chosen.unwrap_or_else(|| {
                let id = templates.len();
                templates.push(row_words(row).into_iter().map(Some).collect());
                members.push(id);
                id
            })
        } else {
            let id = templates.len();
            templates.push(row_words(row).into_iter().map(Some).collect());
            members.push(id);
            id
        };
        for (i, token) in templates[cluster].iter_mut().enumerate() {
            if token.as_deref() != Some(row.token(i)) {
                *token = None;
            }
        }
        assignments.push(cluster);
    }
    assignments
}

fn mdl2_clusters(rows: &[Row]) -> Vec<usize> {
    // The original MDL compares every row in a block at once. Keeping token
    // counts together gives its same cost scorer less heterogeneous groups.
    let mut groups = HashMap::<usize, usize>::new();
    rows.iter()
        .map(|row| {
            let next = groups.len();
            *groups.entry(row.tokens.len()).or_insert(next)
        })
        .collect()
}

fn spell_clusters(rows: &[Row]) -> Vec<usize> {
    // Spell-style online LCS clusters, indexed by the first token as in its
    // prefix tree. Overlap prunes impossible candidates; actual LCS picks the
    // best survivor. Exact repeats reuse their previous cluster immediately.
    let mut templates: Vec<Vec<String>> = Vec::new();
    let mut token_sets: Vec<HashSet<String>> = Vec::new();
    let mut buckets = HashMap::<String, Vec<usize>>::new();
    let mut exact = HashMap::<&str, usize>::new();
    let mut assignments = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(&cluster) = exact.get(row.message.as_str()) {
            assignments.push(cluster);
            continue;
        }
        // Borrow the incoming tokens until a new cluster needs to own them.
        let words: Vec<_> = (0..row.tokens.len()).map(|i| row.token(i)).collect();
        let key = words.first().copied().unwrap_or("").to_owned();
        let members = buckets.entry(key).or_default();
        let mut best = None;
        let mut best_lcs = Vec::new();
        for &i in members.iter() {
            // Token overlap is an upper bound on LCS length. It cheaply
            // rejects impossible matches, then actual LCS picks the cluster.
            let overlap = words
                .iter()
                .filter(|word| token_sets[i].contains(**word))
                .count();
            if overlap * 2 < words.len().max(1) || overlap <= best_lcs.len() {
                continue;
            }
            let pattern: Vec<_> = templates[i].iter().map(String::as_str).collect();
            let kept = lcs_indices(&pattern, &words);
            if kept.len() > best_lcs.len() && kept.len() * 2 >= words.len().max(1) {
                best = Some(i);
                best_lcs = kept;
            }
        }
        let cluster = if let Some(id) = best {
            templates[id] = best_lcs
                .into_iter()
                .map(|i| templates[id][i].clone())
                .collect();
            token_sets[id] = templates[id].iter().cloned().collect();
            id
        } else if members.len() < 8 {
            templates.push(words.iter().map(|word| (*word).to_owned()).collect());
            token_sets.push(words.iter().map(|word| (*word).to_owned()).collect());
            let id = templates.len() - 1;
            members.push(id);
            id
        } else {
            // This row fits no available Spell cluster in this block.
            assignments.push(usize::MAX);
            continue;
        };
        assignments.push(cluster);
        exact.insert(row.message.as_str(), cluster);
    }
    assignments
}

fn sequitur_clusters(rows: &[Row]) -> Vec<usize> {
    // Bounded Sequitur-style digram grammar. Repeated adjacent symbols become
    // shared rules; each row is grouped by its strongest surviving rule.
    let mut symbols = HashMap::<String, u32>::new();
    let mut sequences = Vec::with_capacity(rows.len());
    for row in rows {
        let mut sequence = Vec::new();
        for i in 0..row.tokens.len() {
            let token = row.token(i);
            let next = symbols.len() as u32;
            sequence.push(*symbols.entry(token.to_owned()).or_insert(next));
        }
        sequences.push(sequence);
    }
    let first_rule = symbols.len() as u32;
    let mut rule_children = Vec::<[u32; 2]>::new();
    let mut rule_lengths = Vec::<usize>::new();
    for rule_number in 0..12 {
        let mut counts = HashMap::<(u32, u32), usize>::new();
        for sequence in &sequences {
            for pair in sequence.windows(2) {
                *counts.entry((pair[0], pair[1])).or_default() += 1;
            }
        }
        let Some((pair, count)) = counts
            .into_iter()
            .max_by_key(|&(pair, count)| (count, pair))
        else {
            break;
        };
        if count < 2 {
            break;
        }
        let rule = first_rule + rule_number;
        // Retain the hierarchy: a later rule may hide an earlier rule inside
        // it, but both remain available when grouping messages.
        let length = |symbol: u32| {
            if symbol < first_rule {
                1
            } else {
                rule_lengths[(symbol - first_rule) as usize]
            }
        };
        rule_lengths.push(length(pair.0) + length(pair.1));
        rule_children.push([pair.0, pair.1]);
        for sequence in &mut sequences {
            let mut rewritten = Vec::with_capacity(sequence.len());
            let mut i = 0;
            while i < sequence.len() {
                if i + 1 < sequence.len() && (sequence[i], sequence[i + 1]) == pair {
                    rewritten.push(rule);
                    i += 2;
                } else {
                    rewritten.push(sequence[i]);
                    i += 1;
                }
            }
            *sequence = rewritten;
        }
    }
    let used_by_row: Vec<_> = sequences
        .iter()
        .map(|sequence| {
            let mut used = vec![false; rule_children.len()];
            let mut pending: Vec<_> = sequence
                .iter()
                .copied()
                .filter(|&symbol| symbol >= first_rule)
                .collect();
            while let Some(symbol) = pending.pop() {
                let index = (symbol - first_rule) as usize;
                if used[index] {
                    continue;
                }
                used[index] = true;
                pending.extend(
                    rule_children[index]
                        .into_iter()
                        .filter(|&child| child >= first_rule),
                );
            }
            used
        })
        .collect();
    let support: Vec<_> = (0..rule_children.len())
        .map(|rule| used_by_row.iter().filter(|used| used[rule]).count())
        .collect();
    used_by_row
        .into_iter()
        .map(|used| {
            // A rule repeated only inside one message cannot make a shared
            // template; prefer the longest rule appearing in at least two rows.
            (0..used.len())
                .filter(|&rule| used[rule] && support[rule] >= 2)
                .max_by_key(|&rule| (rule_lengths[rule], support[rule], rule))
                .map_or(usize::MAX, |rule| (first_rule as usize) + rule)
        })
        .collect()
}

type Rendering = (String, Vec<String>);

struct Variant {
    // Original positions stay stable; M and D are the comparison variants.
    tag: [u8; 9],
    template: String,
    fields: Vec<String>,
    // Display-only metadata; stored templates retain positional {} markers.
    field_types: Option<Vec<ValueType>>,
}

fn typed_template(template: &str, types: &[ValueType]) -> String {
    // Respect escaped literal braces/backslashes when decorating placeholders.
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    let mut types = types.iter();
    while let Some(ch) = chars.next() {
        result.push(ch);
        if ch == '\\' {
            if let Some(escaped) = chars.next() {
                result.push(escaped);
            }
        } else if ch == '{' && chars.peek() == Some(&'}') {
            if let Some(kind) = types.next() {
                result.push_str(kind.label());
            }
            result.push(chars.next().unwrap());
        }
    }
    result
}

fn render_algorithm(rows: &[Row], algorithm: Algorithm) -> Vec<Option<Rendering>> {
    if matches!(algorithm, Algorithm::Drain2) {
        return drain2::State::default().render_block(rows);
    }
    let assignments = match algorithm {
        Algorithm::Drain => drain_clusters(rows),
        Algorithm::Mdl2 => mdl2_clusters(rows),
        Algorithm::Spell => spell_clusters(rows),
        Algorithm::Sequitur => sequitur_clusters(rows),
        _ => vec![0; rows.len()],
    };
    let mut groups = HashMap::<usize, Vec<usize>>::new();
    for (i, cluster) in assignments.iter().enumerate() {
        if *cluster != usize::MAX {
            groups.entry(*cluster).or_default().push(i);
        }
    }
    let mut rendered = vec![None; rows.len()];
    for indices in groups.values() {
        if indices.len() < 2 {
            continue;
        }
        let selected: Vec<_> = indices.iter().map(|&i| &rows[i]).collect();
        let candidates = match algorithm {
            Algorithm::Msa | Algorithm::Spell => msa_candidates(&selected),
            _ => aligned_candidates(&selected),
        };
        if let Some((template, fields)) = render_template(&selected, candidates) {
            for (&index, fields) in indices.iter().zip(fields) {
                rendered[index] = Some((template.clone(), fields));
            }
        }
    }
    rendered
}

fn print_group(
    rows: &[Row],
    full_block: bool,
    selected: &[bool; 9],
    colored: bool,
    carry: bool,
    preextract: bool,
    output: &mut impl Write,
    first: &mut bool,
    timings: &mut [Timing; 9],
    learned: &mut [LearnedState; 9],
) -> io::Result<()> {
    let mut variants: Vec<Vec<Variant>> = (0..rows.len()).map(|_| Vec::new()).collect();
    for algorithm in Algorithm::ALL {
        let position = algorithm.index();
        if !selected[position] {
            continue;
        }
        // Time the complete per-algorithm block pass: cached matches, clustering,
        // candidate alignment, chosen_anchors scoring, template construction,
        // and learned-state updates. Drain2 includes regex extraction/cache
        // work. Every method includes optional masks and capture type tracking.
        // TSV parsing, size/diversity accounting, merging, and
        // output are shared/reporting work outside this interval.
        let started = Instant::now();
        // Each method pays for its own preprocessing, even when identical
        // masks could be shared. Raw prefix matching needs only message bytes;
        // tokenizing it wastes work without contributing to the extraction.
        // The raw baseline only needs preprocessing when requested, to label
        // an exact whole-message capture's type.
        let raw_baseline = matches!(algorithm, Algorithm::None) && !preextract;
        let raw_prefix = matches!(algorithm, Algorithm::Prefix) && !preextract;
        let prepared = if raw_baseline || raw_prefix {
            Vec::new()
        } else {
            preextract::prepare_rows(rows, preextract)
        };
        let algorithm_rows = if raw_baseline || raw_prefix {
            rows
        } else {
            &prepared
        };
        let mut fresh = LearnedState::default();
        let state = if carry {
            &mut learned[position]
        } else {
            &mut fresh
        };
        let rendered = state.render_block(algorithm_rows, algorithm);
        // Type labels also feed the block archetype metric. Keep this capture
        // bookkeeping inside the timed algorithm pass, before output merging.
        let field_types: Vec<Option<Vec<ValueType>>> = if !preextract {
            Vec::new()
        } else if matches!(algorithm, Algorithm::Drain2) {
            state.drain2_field_types().to_vec()
        } else {
            rendered
                .iter()
                .zip(algorithm_rows)
                .map(|(result, row)| {
                    result
                        .as_ref()
                        .map(|(template, fields)| learned::capture_types(template, row, fields))
                })
                .collect()
        };
        let elapsed = started.elapsed();
        timings[position].record_block(elapsed, rows.len());
        let mut summary = BlockSummary::default();
        let mut block_sizes = StringSizes::default();
        // Batch the dictionary work under one timer per block, so timing
        // overhead does not grow with the number of templates/value fields.
        // Both report-global pools and the full-block codec pool are counted.
        // Sorting, concatenation, diversity, merging and output stay outside.
        let dedup_started = Instant::now();
        for (row, result) in rows.iter().zip(&rendered) {
            match result {
                Some((template, fields)) if !matches!(algorithm, Algorithm::None) => {
                    timings[position].sizes.record(template, fields);
                    if full_block {
                        block_sizes.record(template, fields);
                    }
                }
                _ => {
                    // Raw baselines and failed extraction store whole strings.
                    timings[position].sizes.record_value(&row.message);
                    if full_block {
                        block_sizes.record_value(&row.message);
                    }
                }
            }
        }
        timings[position].record_dedup_block(dedup_started.elapsed(), rows.len());
        for (row_index, result) in rendered.into_iter().enumerate() {
            let Some((template, fields)) = result else {
                if full_block {
                    // Failed extraction stores the whole original message.
                    summary.record(
                        None,
                        std::iter::once(rows[row_index].message.as_str()),
                        false,
                    );
                }
                continue;
            };
            // Diversity and output merging have their own work, outside all
            // three timers. Dictionary accounting was completed above.
            if matches!(algorithm, Algorithm::None) {
                if full_block {
                    if let Some(Some(types)) = field_types.get(row_index) {
                        summary.record_types(None, types);
                    } else {
                        summary.record(
                            None,
                            std::iter::once(rows[row_index].message.as_str()),
                            false,
                        );
                    }
                }
            } else {
                if full_block {
                    if let Some(Some(types)) = field_types.get(row_index) {
                        summary.record_types(Some(&template), types);
                    } else {
                        summary.record(Some(&template), fields.iter().map(String::as_str), false);
                    }
                }
            }
            let matching = variants[row_index]
                .iter_mut()
                // Typed placeholders are part of the displayed template.
                // Keep differing capture schemas on separate output lines.
                .find(|item| {
                    item.template == template
                        && item.fields == fields
                        && item.field_types == field_types.get(row_index).cloned().flatten()
                });
            if let Some(existing) = matching {
                existing.tag[position] = TAG_BYTES[position];
                if let Some(Some(types)) = field_types.get(row_index) {
                    existing.field_types = Some(types.clone());
                }
            } else {
                let mut tag = [b' '; 9];
                tag[position] = TAG_BYTES[position];
                variants[row_index].push(Variant {
                    tag,
                    template,
                    fields,
                    field_types: field_types.get(row_index).cloned().flatten(),
                });
            }
        }
        if full_block {
            timings[position].record_full_block(&summary);
            // Pool sorting/concatenation are outside all three timers.
            // Time only LZ4 compression of the block-local template/value pool.
            let payload = block_sizes.unique_payload();
            if !payload.is_empty() {
                let started = Instant::now();
                std::hint::black_box(lz4_flex::block::compress(&payload));
                timings[position].compression_elapsed += started.elapsed();
            }
        }
    }
    let mut printed_group = false;
    for (row, variants) in rows.iter().zip(variants) {
        for variant in variants {
            if !printed_group {
                if !*first {
                    writeln!(output)?;
                }
                *first = false;
                printed_group = true;
            }
            output.write_all(&variant.tag)?;
            write!(output, "\t{}\t{}\t", row.archetype, row.target)?;
            // Keep ANSI codes inside the template column so tabs stay plain.
            let template = variant.field_types.as_ref().map_or_else(
                || variant.template.clone(),
                |types| typed_template(&variant.template, types),
            );
            if colored {
                write!(output, "{TEMPLATE_COLOR}{template}{COLOR_RESET}")?;
            } else {
                write!(output, "{template}")?;
            }
            for field in variant.fields {
                write!(output, "\t{}", escape(&field))?;
            }
            writeln!(output)?;
        }
    }
    Ok(())
}

fn print_archetype(
    rows: &[Row],
    block_size: Option<usize>,
    selected: &[bool; 9],
    colored: bool,
    carry: bool,
    preextract: bool,
    output: &mut impl Write,
    first: &mut bool,
    timings: &mut [Timing; 9],
) -> io::Result<()> {
    if rows.len() < 2 {
        // Singleton archetypes are omitted from display and timing, but their
        // original messages still occupy storage for every selected method.
        for algorithm in Algorithm::ALL {
            let index = algorithm.index();
            if selected[index] {
                for row in rows {
                    timings[index].sizes.record_value(&row.message);
                }
            }
        }
        return Ok(());
    }
    let size = block_size.unwrap_or(rows.len());
    // Learned templates and examples belong to one archetype; block boundaries
    // do not reset them, while a new archetype starts with fresh state.
    let mut learned = std::array::from_fn(|_| LearnedState::default());
    for block in rows.chunks(size) {
        print_group(
            block,
            block.len() == size,
            selected,
            colored,
            carry,
            preextract,
            output,
            first,
            timings,
            &mut learned,
        )?;
    }
    Ok(())
}

fn parse_preextract(value: &str) -> Result<bool, String> {
    // Boolean options with attached values must honor explicit false instead
    // of treating every occurrence of the option name as an enabled flag.
    match value {
        "" | "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        _ => Err(format!(
            "invalid --preextract value: {value}; expected true or false"
        )),
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut path = None;
    let mut block_size = None;
    let mut selected = [true; 9];
    let mut colored = false;
    let mut carry = true;
    let mut preextract = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let (name, attached) = arg.split_once('=').unwrap_or((&arg, ""));
        match name {
            "--algo" | "--algorithm" => {
                let value = if attached.is_empty() {
                    args.next().ok_or("missing --algo value")?
                } else {
                    attached.to_owned()
                };
                selected = [false; 9];
                for name in value.split(',') {
                    if name == "all" {
                        selected = [true; 9];
                    } else {
                        let algorithm = Algorithm::parse(name)
                            .ok_or_else(|| format!("unknown algorithm: {name}"))?;
                        selected[algorithm.index()] = true;
                    }
                }
            }
            "--block_size" | "--block-size" => {
                let value = if attached.is_empty() {
                    args.next().ok_or("missing --block_size value")?
                } else {
                    attached.to_owned()
                };
                let size: usize = value.parse()?;
                if size == 0 {
                    return Err("--block_size must be positive".into());
                }
                block_size = Some(size);
            }
            "--colored" => colored = true,
            "--no-carry" => carry = false,
            "--preextract" => preextract = parse_preextract(attached)?,
            "--help" | "-h" => {
                println!(
                    "Usage: print_strings_mdl [--algo=msa,seq,mdl,mdl2,drain,drain2,spell,prefix,none] [--block_size=ROWS] [--colored] [--no-carry] [--preextract[=true|false]] DUMP.tsv"
                );
                println!(
                    "Default: all nine algorithms. Output: tag<TAB>archetype<TAB>target<TAB>template<TAB>value..."
                );
                println!(
                    "Tag positions: aqmdspnMD; spaces mean that method did not produce this row."
                );
                println!(
                    "{{}} marks each value slot. Methods without a useful template emit no row."
                );
                println!(
                    "With --preextract, all methods print slot types: {{module}}, {{path}}, {{ip}}, {{number}}, {{string}}."
                );
                println!("--colored wraps only the template column in ANSI cyan.");
                println!("--no-carry resets learned templates after every block.");
                println!(
                    "--preextract enables regex masks and capture types, including for Drain2; preprocessing is timed."
                );
                println!(
                    "Omit --preextract or pass --preextract=false to disable regex masking; ordinary tokenization still runs."
                );
                println!(
                    "Completion reports timing and sizes to stderr and the example’s formats.result."
                );
                return Ok(());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}").into()),
            _ if path.is_none() => path = Some(arg),
            _ => return Err("expected one TSV path".into()),
        }
    }
    let path = path.ok_or("missing TSV path; use --help")?;
    let source = BufReader::new(File::open(path)?);
    let mut output = BufWriter::new(io::stdout().lock());
    let mut rows: Vec<Row> = Vec::new();
    let mut first = true;
    let mut timings = std::array::from_fn(|_| Timing::default());
    for line in source.lines() {
        let line = line?;
        let mut columns = line.splitn(3, '\t');
        let archetype = columns.next().unwrap_or("");
        let target = columns.next().ok_or("missing target column")?;
        let message = columns.next().ok_or("missing message column")?;
        if !rows.is_empty() && rows[0].archetype != archetype {
            print_archetype(
                &rows,
                block_size,
                &selected,
                colored,
                carry,
                preextract,
                &mut output,
                &mut first,
                &mut timings,
            )?;
            rows.clear();
        }
        let message = unescape(message);
        rows.push(Row {
            archetype: archetype.to_owned(),
            target: target.to_owned(),
            message,
            // Preprocessing happens inside each algorithm's block timer.
            tokens: Vec::new(),
            masked: Vec::new(),
        });
    }
    print_archetype(
        &rows,
        block_size,
        &selected,
        colored,
        carry,
        preextract,
        &mut output,
        &mut first,
        &mut timings,
    )?;
    // Detect a closed output pipe before reporting timings for an incomplete run.
    output.flush()?;
    print_timings(&selected, &timings, preextract)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        // Pagers and `head` may close stdout before the TSV is fully written.
        // Treat that normal consumer behavior as a successful early exit.
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|io_error| io_error.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        eprintln!("print_strings_mdl: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(archetype: &str, message: &str) -> Row {
        Row {
            archetype: archetype.to_owned(),
            target: "test".to_owned(),
            message: message.to_owned(),
            tokens: tokenize(message),
            masked: Vec::new(),
        }
    }

    fn token_row(words: &[&str]) -> Row {
        // Synthetic tokens isolate clustering decisions from punctuation and
        // whitespace tokenization in the real TSV format.
        let mut message = String::new();
        let mut tokens = Vec::new();
        for word in words {
            let start = message.len();
            message.push_str(word);
            tokens.push(Token {
                start,
                end: message.len(),
            });
        }
        Row {
            archetype: "a".into(),
            target: "test".into(),
            message,
            tokens,
            masked: Vec::new(),
        }
    }

    #[test]
    fn learned_templates_cross_blocks_and_refine_within_a_block() {
        // Every extraction method can form a template using an earlier block's
        // unmatched example, even when both blocks contain only one message.
        for algorithm in Algorithm::ALL
            .into_iter()
            .filter(|algo| !matches!(algo, Algorithm::None))
        {
            let mut learner = LearnedState::default();
            assert!(learner.render_block(&[row("a", "job ID(1) done")], algorithm)[0].is_none());
            assert!(
                learner.render_block(&[row("a", "job ID(2) done")], algorithm)[0].is_some(),
                "{}",
                algorithm.name()
            );
        }
        let mut learned = LearnedState::default();
        learned.render_block(&[row("a", "job ID(1) done")], Algorithm::Mdl);
        let next = learned.render_block(&[row("a", "job ID(2) done")], Algorithm::Mdl);
        assert_eq!(next[0].as_ref().unwrap().0, "job ID({}) done");
        assert_eq!(next[0].as_ref().unwrap().1, ["2"]);
        let repeated = learned.render_block(
            &[row("a", "job ID(3) done"), row("a", "job ID(3) done")],
            Algorithm::Mdl,
        );
        assert!(
            repeated
                .iter()
                .all(|item| item.as_ref().unwrap().0 == "job ID(3) done")
        );
        assert!(
            repeated
                .iter()
                .all(|item| item.as_ref().unwrap().1.is_empty())
        );
        // The caller creates a fresh learner at each archetype boundary.
        assert!(
            LearnedState::default().render_block(&[row("b", "job ID(3) done")], Algorithm::Mdl)[0]
                .is_none()
        );
    }

    #[test]
    fn shared_preextraction_preserves_whole_fields_for_every_template_method() {
        // Different module depths and generic lengths become one atomic token,
        // while literal braces, tabs and original UTF-8 values remain lossless.
        let raw = [
            row("a", "added plugin: foo::Plugin"),
            row("a", "added plugin: baz::deep::Plugin<Vec<other::Type>>"),
        ];
        let prepared = preextract::prepare_rows(&raw, true);
        assert_eq!(prepared[0].tokens.len(), prepared[1].tokens.len());
        for algorithm in Algorithm::ALL
            .into_iter()
            .filter(|algo| !matches!(algo, Algorithm::None))
        {
            let mut state = LearnedState::default();
            let results = state.render_block(&prepared, algorithm);
            for (original, result) in raw.iter().zip(results) {
                let (template, fields) =
                    result.unwrap_or_else(|| panic!("{} omitted module path", algorithm.name()));
                assert_eq!(template, "added plugin: {}", "{}", algorithm.name());
                assert_eq!(format!("added plugin: {}", fields[0]), original.message);
            }
            let repeated = preextract::prepare_rows(
                &[
                    row("a", "added plugin: same::Plugin"),
                    row("a", "added plugin: same::Plugin"),
                ],
                true,
            );
            assert!(
                state
                    .render_block(&repeated, algorithm)
                    .iter()
                    .all(|output| output
                        .as_ref()
                        .is_some_and(|(template, _)| template == "added plugin: {}")),
                "{} promoted a regex-masked field",
                algorithm.name()
            );
        }
    }

    #[test]
    fn cached_patterns_cannot_split_regex_spans() {
        // A literal inside a generic type must not become the cached delimiter
        // between two slots. Matching may be declined and rediscovered.
        let source = preextract::prepare_rows(&[row("a", "plug foo::A<bar::B done> done")], true);
        let pattern = learned::test_pattern_captures("plug {} done{}", &source[0]);
        assert!(pattern.is_none());
    }

    #[test]
    fn compression_uses_counted_bytes_in_stable_order() {
        // Escaped template text and whole-message values must share the same
        // byte accounting as the compression corpus, regardless of insertion.
        let mut first = StringSizes::default();
        first.record("é\\t{}", &["repeated".into(), "repeated".into()]);
        first.record_value("raw message");
        let mut second = StringSizes::default();
        second.record_value("raw message");
        second.record("é\\t{}", &["repeated".into()]);
        let payload = first.unique_payload();
        assert_eq!(payload, second.unique_payload());
        assert_eq!(
            payload.len() as u64,
            first.unique_template_bytes + first.unique_value_bytes
        );
        let compressed = lz4_flex::block::compress(&payload);
        assert_eq!(first.lz4_bytes(), compressed.len() as u64);
        assert_eq!(
            lz4_flex::block::decompress(&compressed, payload.len()).unwrap(),
            payload
        );
        assert_eq!(StringSizes::default().lz4_bytes(), 0);
    }

    #[test]
    fn partial_blocks_weight_timing_by_message_count() {
        // A final one-row block has less weight than a preceding full block.
        let mut timing = Timing::default();
        timing.record_block(Duration::from_millis(2), 2);
        timing.record_block(Duration::from_millis(10), 1);
        timing.record_dedup_block(Duration::from_millis(3), 2);
        timing.record_dedup_block(Duration::from_millis(12), 1);
        assert_eq!(timing.blocks, 2);
        assert_eq!(timing.rows, 3);
        assert!((timing.weighted_avg_ms_per_block().unwrap() - 14.0 / 3.0).abs() < 1e-9);
        assert!((timing.weighted_avg_dedup_ms_per_block().unwrap() - 6.0).abs() < 1e-9);
        assert!(Timing::default().weighted_avg_ms_per_block().is_none());
        assert!(
            Timing::default()
                .weighted_avg_dedup_ms_per_block()
                .is_none()
        );
    }

    #[test]
    fn byte_columns_use_non_singleton_rows() {
        // The singleton still occupies storage, but does not enlarge the
        // divisor used to compare methods on the processable archetypes.
        let mut timings: [Timing; 9] = std::array::from_fn(|_| Timing::default());
        let mut output = Vec::new();
        let mut first = true;
        let mut selected = [false; 9];
        selected[Algorithm::None.index()] = true;
        print_archetype(
            &[row("single", "123456")],
            Some(2),
            &selected,
            false,
            true,
            false,
            &mut output,
            &mut first,
            &mut timings,
        )
        .unwrap();
        print_archetype(
            &[row("multi", "abc"), row("multi", "def")],
            Some(2),
            &selected,
            false,
            true,
            false,
            &mut output,
            &mut first,
            &mut timings,
        )
        .unwrap();
        let timing = &timings[Algorithm::None.index()];
        assert_eq!(timing.rows, 2);
        assert_eq!(timing.sizes.value_bytes, 12);
        assert_eq!(
            format_bytes_per_row(timing.sizes.value_bytes, timing.rows),
            "6.00"
        );
        assert_eq!(format_bytes_per_row(1, 0), "n/a");
    }

    #[test]
    fn report_numbers_have_three_significant_figures_and_aligned_columns() {
        assert_eq!(format_ms(0.0), "0");
        assert_eq!(format_ms(0.012345), "0.0123");
        assert_eq!(format_ms(12.345), "12.3");
        assert_eq!(format_ms(999.9), "1000");
        assert_eq!(format_ms(12345.6), "12346");
        assert_eq!(format_ms(0.00012345), "0.000123");
        assert!(matches!(Algorithm::parse("seq"), Some(Algorithm::Sequitur)));
        assert_eq!(Algorithm::Sequitur.name(), "seq");

        // Measurement columns line up despite labels and values of different lengths.
        let mut output = Vec::new();
        write_aligned_table(
            &mut output,
            &["algo", "count"],
            &[
                vec![ReportCell::label("seq"), ReportCell::number("1", 1.0)],
                vec![
                    ReportCell::label("prefix"),
                    ReportCell::number("123", 123.0),
                ],
            ],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "algo    count\nseq         \x1b[38;2;80;165;255m1\x1b[0m\nprefix    \x1b[38;2;235;75;75m123\x1b[0m\n"
        );
        assert_eq!(report_rgb(5.0, 5.0, 5.0), [158, 120, 165]);
        assert_eq!(report_rgb(0.0, 0.0, 5.0), REPORT_ZERO_RGB);
        assert_eq!(report_rgb(0.0, 0.0, 0.0), REPORT_ZERO_RGB);
        let mut number = Vec::new();
        write_colored_number(&mut number, "01010", REPORT_HIGH_RGB).unwrap();
        assert_eq!(
            String::from_utf8(number).unwrap(),
            "\x1b[38;2;90;90;90m0\x1b[38;2;235;75;75m1010\x1b[0m"
        );
    }

    #[test]
    fn timing_columns_share_precision_without_losing_small_measurements() {
        let mut rows = vec![
            vec![ReportCell::number(format_ms(1234.0), 1234.0)],
            vec![ReportCell::number(format_ms(0.012345), 0.012345)],
            vec![ReportCell::number(format_ms(0.0), 0.0)],
            vec![ReportCell::label("n/a")],
        ];
        align_decimal_places(&mut rows, &[0]);
        assert_eq!(
            rows.iter()
                .map(|row| row[0].text.as_str())
                .collect::<Vec<_>>(),
            ["1234.0000", "0.0123", "0.0000", "n/a"]
        );
    }

    #[test]
    fn decimal_points_align_with_different_precision_and_suffixes() {
        // Strip terminal color codes before checking the visible alignment.
        let mut output = Vec::new();
        write_aligned_table(
            &mut output,
            &["algo", "size"],
            &[
                vec![ReportCell::label("a"), ReportCell::number("1.20", 1.2)],
                vec![
                    ReportCell::label("b"),
                    ReportCell::number("123.4K", 123400.0),
                ],
                vec![ReportCell::label("c"), ReportCell::number("0", 0.0)],
            ],
        )
        .unwrap();
        let colored = String::from_utf8(output).unwrap();
        let plain: String = colored
            .split("\x1b[")
            .enumerate()
            .map(|(i, part)| {
                if i == 0 {
                    part
                } else {
                    part.split_once('m').unwrap().1
                }
            })
            .collect();
        let lines: Vec<_> = plain.lines().collect();
        assert_eq!(lines[1].find('.'), lines[2].find('.'));
        assert_eq!(lines[1].len(), lines[2].len());
        assert_eq!(lines[2].len(), lines[3].len());
    }

    #[test]
    fn new_variants_change_grouping_without_changing_original_methods() {
        let samples = [row("a", "10 sent"), row("a", "20 sent")];
        assert_ne!(drain_clusters(&samples)[0], drain_clusters(&samples)[1]);
        let prepared = preextract::prepare_rows(&samples, true);
        let rendered = render_algorithm(&prepared, Algorithm::Drain2);
        assert_eq!(rendered[0], Some(("{} sent".into(), vec!["10".into()])));
        assert_eq!(rendered[1], Some(("{} sent".into(), vec!["20".into()])));

        let mixed = [
            row("a", "job 1 done"),
            row("a", "job 2 done"),
            row("a", "job done"),
        ];
        assert_eq!(mdl2_clusters(&mixed)[0], mdl2_clusters(&mixed)[1]);
        assert_ne!(mdl2_clusters(&mixed)[0], mdl2_clusters(&mixed)[2]);
        assert!(
            mdl2_clusters(&mixed)
                .iter()
                .all(|&group| group != usize::MAX)
        );
    }

    #[test]
    fn spell_uses_actual_lcs_and_seq_requires_cross_message_rule() {
        let spell_rows = [
            token_row(&["a", "4", "3", "2", "1"]),
            token_row(&["a", "x", "1", "2", "3", "z"]),
            token_row(&["a", "1", "2", "3", "4"]),
        ];
        let clusters = spell_clusters(&spell_rows);
        assert_ne!(clusters[0], clusters[1]);
        assert_eq!(clusters[1], clusters[2]);

        let seq_rows = [
            token_row(&["x", "y", "x", "y"]),
            token_row(&["a", "b", "c"]),
        ];
        assert_eq!(sequitur_clusters(&seq_rows), [usize::MAX; 2]);
    }

    #[test]
    fn every_extracted_template_reconstructs_its_message() {
        // The alternate grouping and rule paths must still emit exact fields.
        let samples = vec![
            row("a", "user 10 opened /tmp/a"),
            row("a", "user 20 opened /tmp/b"),
            row("a", "user 30 opened /tmp/c"),
        ];
        for algorithm in Algorithm::ALL
            .into_iter()
            .filter(|algo| !matches!(algo, Algorithm::None | Algorithm::Prefix))
        {
            let rendered = render_algorithm(&samples, algorithm);
            let mut emitted = 0;
            for (source, result) in samples.iter().zip(rendered) {
                let Some((template, fields)) = result else {
                    continue;
                };
                emitted += 1;
                let mut parts = template.split("{}");
                let mut rebuilt = parts.next().unwrap_or("").to_owned();
                for (value, literal) in fields.iter().zip(parts) {
                    rebuilt.push_str(value);
                    rebuilt.push_str(literal);
                }
                assert_eq!(rebuilt, source.message, "{}", algorithm.name());
            }
            assert!(emitted > 0, "{}", algorithm.name());
        }
    }

    #[test]
    fn comparison_variants_keep_fixed_tag_positions() {
        // All selected methods may merge onto one printed row, but their
        // positions remain stable for downstream readers of the TSV.
        let rows = [row("a", "job 1 done"), row("a", "job 2 done")];
        let mut selected = [false; 9];
        for algorithm in [
            Algorithm::Mdl,
            Algorithm::Mdl2,
            Algorithm::Drain,
            Algorithm::Drain2,
        ] {
            selected[algorithm.index()] = true;
        }
        let mut output = Vec::new();
        let mut first = true;
        let mut timings = std::array::from_fn(|_| Timing::default());
        print_archetype(
            &rows,
            None,
            &selected,
            false,
            true,
            false,
            &mut output,
            &mut first,
            &mut timings,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let tags: Vec<_> = output
            .lines()
            .map(|line| line.split('\t').next().unwrap())
            .collect();
        assert!(!tags.is_empty());
        assert!(tags.iter().all(|tag| tag.len() == TAG_BYTES.len()));
        assert!(
            tags.iter()
                .any(|tag| tag.contains('m') && tag.contains('M'))
        );
        assert!(
            tags.iter()
                .any(|tag| tag.contains('d') && tag.contains('D'))
        );
    }

    #[test]
    fn preextract_flag_honors_explicit_false() {
        // CLI splits --name=value before dispatch: false/off/0 must remain off,
        // and misspelled values must fail rather than silently enabling masks.
        for value in ["", "true", "on", "1"] {
            assert_eq!(parse_preextract(value), Ok(true));
        }
        for value in ["false", "off", "0"] {
            assert_eq!(parse_preextract(value), Ok(false));
        }
        assert!(parse_preextract("flase").is_err());
    }

    #[test]
    fn all_methods_print_capture_types_only_with_preextraction() {
        // Escaped literal {} and backslashes must not consume type labels.
        assert_eq!(
            typed_template(r"literal \{\} \\{} {}", &[ValueType::Module, ValueType::Ip]),
            r"literal \{\} \\{module} {ip}"
        );
        let rows = [
            row("a", "added plugin: foo::Plugin<bar::A>"),
            row("a", "added plugin: baz::deep::Plugin"),
        ];
        let selected = [true; 9];
        for colored in [false, true] {
            let mut output = Vec::new();
            let mut first = true;
            let mut timings = std::array::from_fn(|_| Timing::default());
            print_archetype(
                &rows,
                Some(2),
                &selected,
                colored,
                true,
                true,
                &mut output,
                &mut first,
                &mut timings,
            )
            .unwrap();
            let output = String::from_utf8(output).unwrap();
            for line in output.lines().filter(|line| !line.is_empty()) {
                let columns: Vec<_> = line.split('\t').collect();
                if columns[0].contains('n') {
                    assert!(columns[3].contains("{string}"));
                } else {
                    assert!(columns[3].contains("added plugin: {module}"), "{line}");
                }
            }
            // All eight template methods agree here. The raw baseline prints
            // its whole message as a string.
            assert_eq!(output.matches("added plugin: {module}").count(), 2);
            assert!(output.lines().any(|line| line.starts_with("aqmdsp MD\t")));
            assert_eq!(output.contains(TEMPLATE_COLOR), colored);
            let timing = &timings[Algorithm::Drain2.index()];
            assert_eq!(
                timing.sizes.templates,
                HashSet::from(["added plugin: {}".into()])
            );
            assert_eq!(timing.distinct_archetypes, 1);
            assert_eq!(timing.sizes.value_count, 2);
        }
        // Type decoration must not depend on merging with a Drain2 output.
        let mut selected = [true; 9];
        selected[Algorithm::Drain2.index()] = false;
        let mut output = Vec::new();
        let mut timings = std::array::from_fn(|_| Timing::default());
        print_archetype(
            &rows,
            Some(2),
            &selected,
            false,
            true,
            true,
            &mut output,
            &mut true,
            &mut timings,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("added plugin: {module}").count(), 2);
        assert!(output.lines().any(|line| line.starts_with("aqmdsp M \t")));

        // The whole-message baseline can be typed when its sole value exactly
        // matches a regex capture, while still storing the untouched message.
        let source = preextract::prepare_rows(&[row("a", "42")], true);
        assert_eq!(
            learned::capture_types("{}", &source[0], &["42".into()]),
            [ValueType::Number]
        );
    }

    #[test]
    fn capture_types_use_exact_spans_and_handle_escaped_literals() {
        // A composite gap must not be mislabeled as a number or module path.
        let source = preextract::prepare_rows(&[row("a", "literal {} \\42 foo::Bar")], true);
        assert_eq!(
            learned::capture_types(
                r"literal \{\} \\{} {}",
                &source[0],
                &["42".into(), "foo::Bar".into()]
            ),
            [ValueType::Number, ValueType::Module],
        );
        assert_eq!(
            learned::capture_types("literal {}", &source[0], &["{} \\42 foo::Bar".into()]),
            [ValueType::String],
        );
    }

    #[test]
    fn unmasked_output_has_plain_placeholders_for_every_method() {
        // This also exercises generic regex extraction in unmasked Drain2.
        let rows = [row("a", "job 1 done"), row("a", "job 2 done")];
        let mut output = Vec::new();
        let mut timings = std::array::from_fn(|_| Timing::default());
        print_archetype(
            &rows,
            Some(2),
            &[true; 9],
            false,
            true,
            false,
            &mut output,
            &mut true,
            &mut timings,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("job {} done"));
        for kind in ["string", "number", "ip", "path", "module"] {
            assert!(!output.contains(&format!("{{{kind}}}")), "{output}");
        }
        assert!(
            output
                .lines()
                .any(|line| line.split('\t').next().unwrap().contains('D'))
        );
    }

    #[test]
    fn field_archetypes_ignore_order_but_keep_type_counts() {
        assert_eq!(value_type(" 192.168.1.2 "), ValueType::Ip);
        assert_eq!(value_type("-12.5"), ValueType::Number);
        assert_eq!(value_type("/tmp/data.log"), ValueType::Path);
        assert_eq!(value_type("app::module"), ValueType::Module);
        assert_eq!(value_type("ordinary text"), ValueType::String);
        assert_eq!(
            value_archetype(["ordinary text", "192.168.1.2"], true),
            value_archetype(["192.168.1.2", "ordinary text"], true)
        );
        assert_ne!(
            value_archetype(["ordinary text", "192.168.1.2"], true),
            value_archetype(["192.168.1.2", "ordinary text", "more text"], true)
        );
        // Untyped methods count only string slots, even for IP-like values.
        assert_eq!(
            value_archetype(["192.168.1.2", "ordinary text"], false),
            [2, 0, 0, 0, 0]
        );
    }

    #[test]
    fn block_diversity_uses_only_complete_blocks() {
        let mut timings: [Timing; 9] = std::array::from_fn(|_| Timing::default());
        let mut selected = [false; 9];
        selected[Algorithm::Mdl.index()] = true;
        selected[Algorithm::None.index()] = true;
        let mut output = Vec::new();
        let mut first = true;
        print_archetype(
            &[
                row("a", "job 1 done"),
                row("a", "job 2 done"),
                row("a", "other message"),
            ],
            Some(2),
            &selected,
            false,
            true,
            false,
            &mut output,
            &mut first,
            &mut timings,
        )
        .unwrap();
        let mdl = &timings[Algorithm::Mdl.index()];
        assert_eq!((mdl.blocks, mdl.full_blocks), (2, 1));
        assert_eq!((mdl.distinct_patterns, mdl.distinct_archetypes), (1, 1));
        assert_eq!(
            mdl.avg_compression_ms_per_full_block(),
            Some(mdl.compression_elapsed.as_secs_f64() * 1000.0)
        );
        let none = &timings[Algorithm::None.index()];
        assert_eq!((none.distinct_patterns, none.distinct_archetypes), (0, 1));

        // A two-row archetype is incomplete when the requested block size is 3.
        let mut partial = std::array::from_fn(|_| Timing::default());
        print_archetype(
            &[row("b", "one"), row("b", "two")],
            Some(3),
            &selected,
            false,
            true,
            false,
            &mut output,
            &mut first,
            &mut partial,
        )
        .unwrap();
        assert_eq!(partial[Algorithm::Mdl.index()].full_blocks, 0);
        assert_eq!(
            partial[Algorithm::Mdl.index()].compression_elapsed,
            Duration::ZERO
        );
        assert_eq!(
            partial[Algorithm::Mdl.index()].avg_compression_ms_per_full_block(),
            None
        );
    }
}
