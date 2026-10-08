//! Inspect dumped messages with five fast, self-contained template strategies.
//! Each row prints: algorithm tag, archetype, target, template, then values for {} slots.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::time::{Duration, Instant};

#[path = "learned.rs"]
mod learned;
use learned::LearnedState;

// Change this ANSI SGR code to tune the template color for a terminal theme.
const TEMPLATE_COLOR: &str = "\x1b[1;36m";
const COLOR_RESET: &str = "\x1b[0m";
// Tune these RGB endpoints to suit the terminal theme. Each numeric report
// column interpolates independently from blue (lower cost) to red (higher cost).
const REPORT_LOW_RGB: [u8; 3] = [80, 165, 255];
const REPORT_HIGH_RGB: [u8; 3] = [235, 75, 75];
const REPORT_ZERO_RGB: [u8; 3] = [90, 90, 90];

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
}

impl Row {
    fn token(&self, index: usize) -> &str {
        let token = self.tokens[index];
        &self.message[token.start..token.end]
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
}

impl Algorithm {
    const ALL: [Self; 7] = [
        Self::Msa,
        Self::Sequitur,
        Self::Mdl,
        Self::Drain,
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
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "msa" => Some(Self::Msa),
            "sequitur" | "seq" => Some(Self::Sequitur),
            "mdl" => Some(Self::Mdl),
            "drain" | "drain3" => Some(Self::Drain),
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
        }
    }
}

#[derive(Default)]
struct Timing {
    elapsed: Duration,
    weighted_ms: f64,
    blocks: u64,
    rows: u64,
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

fn print_timings(selected: &[bool; 7], timings: &[Timing; 7]) -> io::Result<()> {
    // Render once so the colocated artifact and terminal receive identical bytes,
    // including headings, alignment, and ANSI colors, regardless of caller cwd.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/dump_messsage_compression/formats.result");
    let mut report = Vec::new();
    writeln!(report, "Algorithm timing (ms; row-weighted block average):")?;
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
        time_rows.push(vec![
            ReportCell::label(algorithm.name()),
            ReportCell::number(format_ms(total_ms), total_ms),
            average,
        ]);
    }
    align_decimal_places(&mut time_rows, &[1, 2]);
    write_aligned_table(&mut report, &["algo", "total", "avg/blk"], &time_rows)?;
    writeln!(
        report,
        "String sizes (UTF-8 B/non-singleton row; tpl/val deduped; all_val retains duplicates; singleton values included; K/M/G decimal; 3 sig figs):"
    )?;
    let mut size_rows = Vec::new();
    for algorithm in Algorithm::ALL {
        let index = algorithm.index();
        if selected[index] {
            let sizes = &timings[index].sizes;
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
            ]);
        }
    }
    let headers = [
        "algo", "tpl", "val", "all_val", "total", "lz4", "tpls", "vals",
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

fn lcs_indices(left: &[String], right: &[String]) -> Vec<usize> {
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
        let left: Vec<_> = candidates
            .iter()
            .map(|&i| first.token(i).to_owned())
            .collect();
        let right = row_words(row);
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
    let mut candidates = initial_candidates;
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

fn spell_clusters(rows: &[Row]) -> Vec<usize> {
    // Spell-style online LCS clusters, indexed by the first token as in its
    // prefix tree. Token overlap picks one LCS candidate; exact repeats reuse
    // their previous cluster immediately.
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
        let words = row_words(row);
        let key = words.first().cloned().unwrap_or_default();
        let members = buckets.entry(key).or_default();
        let mut best = None;
        let mut best_overlap = 0;
        for &i in members.iter() {
            let overlap = words
                .iter()
                .filter(|word| token_sets[i].contains(*word))
                .count();
            if best.is_none() || overlap > best_overlap {
                best = Some(i);
                best_overlap = overlap;
            }
        }
        let kept = best.map(|i| lcs_indices(&templates[i], &words));
        let matched = kept
            .as_ref()
            .is_some_and(|indices| indices.len() * 2 >= words.len().max(1));
        let cluster = if matched {
            let id = best.unwrap_or(0);
            if let Some(kept) = kept {
                templates[id] = kept.into_iter().map(|i| templates[id][i].clone()).collect();
                token_sets[id] = templates[id].iter().cloned().collect();
            }
            id
        } else if members.len() < 8 {
            templates.push(words.clone());
            token_sets.push(words.iter().cloned().collect());
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
    sequences
        .iter()
        .map(|sequence| {
            sequence
                .iter()
                .copied()
                .filter(|&symbol| symbol >= first_rule)
                .max()
                .map_or(usize::MAX, |rule| rule as usize)
        })
        .collect()
}

type Rendering = (String, Vec<String>);

struct Variant {
    // Positions mean alignment, Sequitur, MDL, Drain, Spell, prefix, none.
    tag: [u8; 7],
    template: String,
    fields: Vec<String>,
}

fn render_algorithm(rows: &[Row], algorithm: Algorithm) -> Vec<Option<Rendering>> {
    let assignments = match algorithm {
        Algorithm::Drain => drain_clusters(rows),
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
    selected: &[bool; 7],
    colored: bool,
    carry: bool,
    output: &mut impl Write,
    first: &mut bool,
    timings: &mut [Timing; 7],
    learned: &mut [LearnedState; 7],
) -> io::Result<()> {
    let mut variants: Vec<Vec<Variant>> = (0..rows.len()).map(|_| Vec::new()).collect();
    for algorithm in Algorithm::ALL {
        let position = algorithm.index();
        if !selected[position] {
            continue;
        }
        // Measure clustering and template extraction, excluding TSV parsing,
        // result merging, and output. Attempts with no result still count.
        let started = Instant::now();
        let rendered = if carry {
            learned[position].render_block(rows, algorithm)
        } else {
            LearnedState::default().render_block(rows, algorithm)
        };
        let elapsed = started.elapsed();
        timings[position].record_block(elapsed, rows.len());
        for (row_index, result) in rendered.into_iter().enumerate() {
            let Some((template, fields)) = result else {
                timings[position]
                    .sizes
                    .record_value(&rows[row_index].message);
                continue;
            };
            // Attribute each method's result before identical output rows are
            // merged. Accounting is outside the algorithm's measured runtime.
            if matches!(algorithm, Algorithm::None) {
                timings[position]
                    .sizes
                    .record_value(&rows[row_index].message);
            } else {
                timings[position].sizes.record(&template, &fields);
            }
            let matching = variants[row_index]
                .iter_mut()
                .find(|item| item.template == template && item.fields == fields);
            if let Some(existing) = matching {
                existing.tag[position] = b"aqmdspn"[position];
            } else {
                let mut tag = [b' '; 7];
                tag[position] = b"aqmdspn"[position];
                variants[row_index].push(Variant {
                    tag,
                    template,
                    fields,
                });
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
            if colored {
                write!(output, "{TEMPLATE_COLOR}{}{COLOR_RESET}", variant.template)?;
            } else {
                write!(output, "{}", variant.template)?;
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
    selected: &[bool; 7],
    colored: bool,
    carry: bool,
    output: &mut impl Write,
    first: &mut bool,
    timings: &mut [Timing; 7],
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
            selected,
            colored,
            carry,
            output,
            first,
            timings,
            &mut learned,
        )?;
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut path = None;
    let mut block_size = None;
    let mut selected = [true; 7];
    let mut colored = false;
    let mut carry = true;
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
                selected = [false; 7];
                for name in value.split(',') {
                    if name == "all" {
                        selected = [true; 7];
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
            "--help" | "-h" => {
                println!(
                    "Usage: print_strings_mdl [--algo=msa,seq,mdl,drain,spell,prefix,none] [--block_size=ROWS] [--colored] [--no-carry] DUMP.tsv"
                );
                println!(
                    "Default: all seven algorithms. Output: tag<TAB>archetype<TAB>target<TAB>template<TAB>value..."
                );
                println!(
                    "Tag positions: aqmdspn; spaces mean that method did not produce this row."
                );
                println!(
                    "{{}} marks each value slot. Methods without a useful template emit no row."
                );
                println!("--colored wraps only the template column in ANSI cyan.");
                println!("--no-carry resets learned templates after every block.");
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
                &mut output,
                &mut first,
                &mut timings,
            )?;
            rows.clear();
        }
        let message = unescape(message);
        let tokens = tokenize(&message);
        rows.push(Row {
            archetype: archetype.to_owned(),
            target: target.to_owned(),
            message,
            tokens,
        });
    }
    print_archetype(
        &rows,
        block_size,
        &selected,
        colored,
        carry,
        &mut output,
        &mut first,
        &mut timings,
    )?;
    // Detect a closed output pipe before reporting timings for an incomplete run.
    output.flush()?;
    print_timings(&selected, &timings)?;
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
        assert!(repeated
            .iter()
            .all(|item| item.as_ref().unwrap().0 == "job ID(3) done"));
        assert!(repeated
            .iter()
            .all(|item| item.as_ref().unwrap().1.is_empty()));
        // The caller creates a fresh learner at each archetype boundary.
        assert!(LearnedState::default()
            .render_block(&[row("b", "job ID(3) done")], Algorithm::Mdl)[0]
            .is_none());
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
        assert_eq!(timing.blocks, 2);
        assert_eq!(timing.rows, 3);
        assert!((timing.weighted_avg_ms_per_block().unwrap() - 14.0 / 3.0).abs() < 1e-9);
        assert!(Timing::default().weighted_avg_ms_per_block().is_none());
    }

    #[test]
    fn byte_columns_use_non_singleton_rows() {
        // The singleton still occupies storage, but does not enlarge the
        // divisor used to compare methods on the processable archetypes.
        let mut timings: [Timing; 7] = std::array::from_fn(|_| Timing::default());
        let mut output = Vec::new();
        let mut first = true;
        let mut selected = [false; 7];
        selected[Algorithm::None.index()] = true;
        print_archetype(
            &[row("single", "123456")],
            Some(2),
            &selected,
            false,
            true,
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
}
