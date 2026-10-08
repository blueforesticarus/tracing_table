//! Optional whole-value regex masks and extraction for the Drain comparison variant.
//! Clustering remains the small token-count/first-token approximation, not a Drain tree.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use super::{Rendering, Row, ValueType, escape_template_literal};

const MAX_CLUSTERS: usize = 128;
const MAX_EXACT_PER_CLUSTER: usize = 256;
// Rust regexes have no recursion. Expand balanced generics to this depth so
// module paths such as Plugin<Vec<foo::Bar>> remain a single masked value.
const GENERIC_DEPTH: usize = 4;

struct Masks {
    // One scanner; ordered alternatives put semantic values before numbers.
    scanner: Regex,
    expressions: [String; 5],
}

fn masks() -> &'static Masks {
    static MASKS: OnceLock<Masks> = OnceLock::new();
    MASKS.get_or_init(|| {
        let mut generic = String::from(r"[^<>\r\n]*");
        for _ in 1..GENERIC_DEPTH {
            generic = format!(r"(?:[^<>\r\n]|<{generic}>)*");
        }
        let segment = format!(r"[\p{{L}}_][\p{{L}}\p{{N}}_]*(?:<{generic}>)?");
        let module = format!(r"{segment}(?:::(?:{segment}|<{generic}>))+");
        let path = String::from(
            r#"(?:[A-Za-z]:[\\/]|\\\\|/|\.{1,2}[\\/]|[\p{L}\p{N}_.-]+[\\/])[^\s"'<>|,;)\]}]+"#,
        );
        let ip = String::from(
            r"(?:[0-9]{1,3}(?:\.[0-9]{1,3}){3}|(?:[0-9A-Fa-f]{0,4}:){2,7}[0-9A-Fa-f]{0,4})",
        );
        let number =
            String::from(r"[-+]?(?:0[xX][0-9A-Fa-f]+|[0-9]+(?:\.[0-9]+)?(?:[eE][-+]?[0-9]+)?)");
        // Let semantic alternatives include leading delimiters outside their
        // captures, so an IP/path following '+' wins at the same search position
        // as a signed numeric candidate. Lazy prefix matching retains full paths.
        // The enum order is also the report's field-type count order.
        let scanner = Regex::new(&format!(
            r"[[:punct:]\p{{P}}\s]*?(?:(?P<module>{module})|(?P<path>{path})|(?P<ip>{ip}))|(?:\A|[[:punct:]\p{{P}}\s])(?P<number>{number})(?:\z|[[:punct:]\p{{P}}\s])"
        ))
        .expect("built-in Drain2 masks are valid regexes");
        Masks {
            scanner,
            expressions: [String::from("(?s:.*?)"), number, ip, path, module],
        }
    })
}

fn valid_span(message: &str, start: usize, end: usize, kind: ValueType) -> bool {
    // Avoid treating digits embedded in identifiers, or part of a longer IP,
    // as standalone values. IP syntax is additionally checked by std's parser.
    let word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let before = message[..start].chars().next_back();
    let after = message[end..].chars().next();
    if kind == ValueType::Number {
        // Both boundaries are enforced by the number branch in the regex.
        return true;
    }
    if before.is_some_and(word) || after.is_some_and(word) {
        return false;
    }
    match kind {
        ValueType::Ip => {
            message[start..end].parse::<std::net::IpAddr>().is_ok()
                && before != Some('.')
                && after != Some('.')
        }
        _ => true,
    }
}

pub(super) fn spans(message: &str) -> impl Iterator<Item = (usize, usize, ValueType)> + '_ {
    // The engine chooses the branch. Revisit a consumed right delimiter so
    // adjacent numbers such as 1,2 both match without lookaround or a second
    // pass. Capture ends strictly advance; delimiter text remains literal.
    let mut cursor = 0;
    std::iter::from_fn(move || {
        loop {
            let captures = masks().scanner.captures_at(message, cursor)?;
            for (name, kind) in [
                ("module", ValueType::Module),
                ("path", ValueType::Path),
                ("ip", ValueType::Ip),
                ("number", ValueType::Number),
            ] {
                if let Some(value) = captures.name(name) {
                    cursor = value.end();
                    if valid_span(message, value.start(), value.end(), kind) {
                        return Some((value.start(), value.end(), kind));
                    }
                    break;
                }
            }
        }
    })
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Part {
    Literal(String),
    Field(ValueType),
}

type Span = (usize, usize, ValueType);

fn prepare(row: &Row) -> (Vec<Part>, Vec<Span>) {
    // The caller owns optional preprocessing for every algorithm. Never scan
    // here: without --preextract the row contains only ordinary literal tokens.
    let mut spans = Vec::new();
    let parts = row
        .tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            if let Some(Some(kind)) = row.masked.get(index) {
                spans.push((token.start, token.end, *kind));
                Part::Field(*kind)
            } else {
                Part::Literal(row.message[token.start..token.end].to_owned())
            }
        })
        .collect();
    (parts, spans)
}

struct Compiled {
    template: String,
    // Whole-value masks already captured every typed field. A second regex
    // is needed only after clustering introduces generic string wildcards.
    regex: Option<Regex>,
    types: Vec<ValueType>,
}

impl Compiled {
    fn new(parts: &[Part]) -> Option<Self> {
        // A wholly variable message has no useful template. Failed regex
        // compilation/extraction is omitted, and storage counts the raw row.
        if !parts.iter().any(|part| {
            matches!(part, Part::Literal(text)
            if text.chars().any(char::is_alphanumeric))
        }) {
            return None;
        }
        let mut template = String::new();
        let mut expression = String::from(r"\A");
        let mut types = Vec::new();
        let mut previous_string = false;
        for part in parts {
            match part {
                Part::Literal(text) => {
                    template.push_str(&escape_template_literal(text));
                    expression.push_str(&regex::escape(text));
                    previous_string = false;
                }
                Part::Field(kind) => {
                    // Adjacent generic wildcards form one string field; their
                    // individual boundaries cannot be inferred from a regex.
                    if previous_string && *kind == ValueType::String {
                        continue;
                    }
                    template.push_str("{}");
                    // Ordinary string extraction must not initialize or run
                    // the semantic masking regex when preprocessing is off.
                    let pattern = if *kind == ValueType::String {
                        "(?s:.*?)"
                    } else {
                        &masks().expressions[*kind as usize]
                    };
                    expression.push_str(&format!("({pattern})"));
                    types.push(*kind);
                    previous_string = *kind == ValueType::String;
                }
            }
        }
        expression.push_str(r"\z");
        let regex = if types.contains(&ValueType::String) {
            Some(Regex::new(&expression).ok()?)
        } else {
            None
        };
        Some(Self {
            template,
            regex,
            types,
        })
    }

    fn extract(&self, message: &str) -> Option<Vec<String>> {
        // Capture only from the original unmodified message. Regex anchors,
        // escaped literals and slot order preserve exact reconstruction.
        let captures = self.regex.as_ref()?.captures(message)?;
        (1..captures.len())
            .map(|index| captures.get(index).map(|value| value.as_str().to_owned()))
            .collect::<Option<Vec<_>>>()
    }
}

struct Cluster {
    parts: Vec<Part>,
    seen: usize,
    compiled: Option<Compiled>,
    attempted: bool,
    // Repeated messages need only one regex extraction until the template
    // changes. Include failed matches, and bound memory for variable streams.
    captures: HashMap<String, Option<Vec<String>>>,
}

#[derive(Default)]
pub(super) struct State {
    clusters: Vec<Cluster>,
    // Rebuilt after eviction; IDs otherwise remain stable throughout a block.
    buckets: HashMap<(usize, Option<Part>), Vec<usize>>,
    pub(super) field_types: Vec<Option<Vec<ValueType>>>,
}

impl State {
    pub(super) fn render_block(&mut self, rows: &[Row]) -> Vec<Option<Rendering>> {
        let mut assignments = Vec::with_capacity(rows.len());
        let mut captured_spans = Vec::with_capacity(rows.len());
        for row in rows {
            let (parts, spans) = prepare(row);
            captured_spans.push(spans);
            let key = (parts.len(), parts.first().cloned());
            let members = self.buckets.entry(key).or_default();
            let best = members
                .iter()
                .filter_map(|&index| {
                    let score = self.clusters[index]
                        .parts
                        .iter()
                        .zip(&parts)
                        .filter(|(a, b)| a == b && !matches!(a, Part::Field(ValueType::String)))
                        .count();
                    (score * 2 >= parts.len().max(1)).then_some((index, score))
                })
                .max_by_key(|&(_, score)| score);
            let index = if let Some((index, _)) = best {
                let cluster = &mut self.clusters[index];
                for (old, new) in cluster.parts.iter_mut().zip(&parts) {
                    if old != new && !matches!(old, Part::Field(ValueType::String)) {
                        *old = Part::Field(ValueType::String);
                        cluster.compiled = None;
                        cluster.attempted = false;
                        cluster.captures.clear();
                    }
                }
                cluster.seen += 1;
                index
            } else {
                let index = self.clusters.len();
                self.clusters.push(Cluster {
                    parts,
                    seen: 1,
                    compiled: None,
                    attempted: false,
                    captures: HashMap::new(),
                });
                members.push(index);
                index
            };
            assignments.push(index);
        }
        self.field_types = vec![None; rows.len()];
        let rendered = rows
            .iter()
            .zip(assignments)
            .enumerate()
            .map(|(row_index, (row, index))| {
                let cluster = &mut self.clusters[index];
                if cluster.seen < 2 {
                    return None;
                }
                if !cluster.attempted {
                    cluster.compiled = Compiled::new(&cluster.parts);
                    cluster.attempted = true;
                }
                let compiled = cluster.compiled.as_ref()?;
                let fields = if compiled.regex.is_none() {
                    // With no generic wildcard, all normalized literal parts
                    // matched exactly. Reuse the original mask regex captures
                    // instead of running a huge regex with hundreds of groups
                    // over a long message for a second time.
                    let spans = &captured_spans[row_index];
                    if !spans
                        .iter()
                        .map(|&(_, _, kind)| kind)
                        .eq(compiled.types.iter().copied())
                    {
                        return None;
                    }
                    spans
                        .iter()
                        .map(|&(start, end, _)| row.message[start..end].to_owned())
                        .collect()
                } else if let Some(cached) = cluster.captures.get(&row.message) {
                    cached.clone()?
                } else {
                    let fields = compiled.extract(&row.message);
                    if cluster.captures.len() >= MAX_EXACT_PER_CLUSTER {
                        cluster.captures.clear();
                    }
                    cluster.captures.insert(row.message.clone(), fields.clone());
                    fields?
                };
                self.field_types[row_index] = Some(compiled.types.clone());
                Some((compiled.template.clone(), fields))
            })
            .collect();
        // Evict only after rendering: current-block assignments must remain
        // valid even when a block discovers more than the cache's capacity.
        if self.clusters.len() > MAX_CLUSTERS {
            self.clusters.drain(..self.clusters.len() - MAX_CLUSTERS);
            self.buckets.clear();
            for (index, cluster) in self.clusters.iter().enumerate() {
                self.buckets
                    .entry((cluster.parts.len(), cluster.parts.first().cloned()))
                    .or_default()
                    .push(index);
            }
        }
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(message: &str) -> Row {
        let source = Row {
            archetype: "a".into(),
            target: "t".into(),
            message: message.into(),
            tokens: Vec::new(),
            masked: Vec::new(),
        };
        // Mask-focused fixtures opt in through the same path as the CLI.
        super::super::preextract::prepare_rows(&[source], true)
            .pop()
            .unwrap()
    }

    #[test]
    fn whole_module_paths_mask_before_tokenization_and_extract_with_regex() {
        let rows = [
            row("added plugin: foo::Plugin"),
            row("added plugin: foo::deep::Plugin<Vec<bar::Thing>>"),
            row("added plugin: foo::Plugin<bar::A, baz::B>"),
        ];
        assert_eq!(prepare(&rows[0]).0, prepare(&rows[1]).0);
        let mut state = State::default();
        let rendered = state.render_block(&rows);
        for (index, output) in rendered.into_iter().enumerate() {
            let (template, values) = output.unwrap();
            assert_eq!(template, "added plugin: {}");
            assert_eq!(
                values,
                [rows[index].message.strip_prefix("added plugin: ").unwrap()]
            );
            assert_eq!(state.field_types[index], Some(vec![ValueType::Module]));
        }
    }

    #[test]
    fn masks_paths_ips_and_numbers_without_masking_identifier_digits() {
        let message = r"open /tmp/file123.txt peer 192.168.1.2 ipv6 [2001:db8::1] win C:\tmp\file.log amount -1.25e+3 hex 0xff worker42";
        let found: Vec<_> = spans(message)
            .map(|(start, end, kind)| (&message[start..end], kind))
            .collect();
        assert_eq!(
            found,
            [
                ("/tmp/file123.txt", ValueType::Path),
                ("192.168.1.2", ValueType::Ip),
                ("2001:db8::1", ValueType::Ip),
                (r"C:\tmp\file.log", ValueType::Path),
                ("-1.25e+3", ValueType::Number),
                ("0xff", ValueType::Number),
            ]
        );
    }

    #[test]
    fn numbers_require_punctuation_or_whitespace_and_lose_to_semantic_masks() {
        // Periods, Unicode quotes, whitespace, underscores and string edges
        // delimit values; identifier letters, combining marks and emoji do not.
        let message = "42 count=17, [23] .31. ‘47’ 5abc abc6 7\u{301} 💡8💡 worker_9";
        let numbers: Vec<_> = spans(message)
            .filter(|&(_, _, kind)| kind == ValueType::Number)
            .map(|(start, end, _)| &message[start..end])
            .collect();
        assert_eq!(numbers, ["42", "17", "23", "31", "47", "9"]);
        let message = "foo-11 abc+12 alpha_13 signed -14; +15";
        let numbers: Vec<_> = spans(message)
            .map(|(start, end, _)| &message[start..end])
            .collect();
        assert_eq!(numbers, ["11", "12", "13", "-14", "+15"]);
        let message = "1,2 3;4\n5:6";
        let numbers: Vec<_> = spans(message)
            .map(|(start, end, _)| &message[start..end])
            .collect();
        assert_eq!(numbers, ["1", "2", "3", "4", "5", "6"]);

        // Leading punctuation lets the higher-priority semantic alternatives
        // compete at the same position as signed numeric candidates.
        let message = "peer +2001:db8::1 read +123/file9 use foo9::Bar2 count 10";
        let values: Vec<_> = spans(message)
            .map(|(start, end, kind)| (&message[start..end], kind))
            .collect();
        assert_eq!(
            values,
            [
                ("2001:db8::1", ValueType::Ip),
                ("123/file9", ValueType::Path),
                ("foo9::Bar2", ValueType::Module),
                ("10", ValueType::Number),
            ]
        );
    }

    #[test]
    fn carried_patterns_keep_regex_types_and_literal_escapes() {
        let mut state = State::default();
        let rows = [
            row("plug {foo::Bar}\tfrom /tmp/a\n"),
            row("plug {baz::deep::Bar}\tfrom ./dir/b\n"),
        ];
        for result in state.render_block(&rows) {
            let (template, values) = result.unwrap();
            assert_eq!(template, r"plug \{{}\}\tfrom {}\n");
            assert_eq!(values.len(), 2);
        }
        let next = row("plug {other::Plugin}\tfrom ../dir/c\n");
        let result = state.render_block(&[next]).pop().unwrap().unwrap();
        assert_eq!(result.1, ["other::Plugin", "../dir/c"]);
        assert_eq!(
            state.field_types[0],
            Some(vec![ValueType::Module, ValueType::Path])
        );
        // With learning reset a single unmatched message remains omitted.
        assert!(State::default().render_block(&[row("plug foo::Bar")])[0].is_none());
    }

    #[test]
    fn changed_templates_invalidate_cached_regex_captures() {
        // An exact message may acquire more fields after its cluster changes.
        let mut state = State::default();
        state.render_block(&[row("user a 1 ended"), row("user a 2 ended")]);
        let next = state.render_block(&[row("user b 3 ended"), row("user a 1 ended")]);
        assert_eq!(
            next[1],
            Some(("user {} {} ended".into(), vec!["a".into(), "1".into()]))
        );
        assert_eq!(
            state.field_types[1],
            Some(vec![ValueType::String, ValueType::Number])
        );
    }

    #[test]
    fn unmasked_rows_have_no_semantic_captures() {
        // Digits at the first token cannot be normalized into the same bucket
        // without the flag; even constant numbers remain literal when off.
        let raw = super::super::preextract::prepare_rows(&[row("10 sent"), row("20 sent")], false);
        let mut state = State::default();
        assert!(state.render_block(&raw).iter().all(Option::is_none));
        let raw = super::super::preextract::prepare_rows(
            &[row("job 10 done"), row("job 10 done")],
            false,
        );
        let output = state.render_block(&raw);
        assert_eq!(output[0], Some(("job 10 done".into(), Vec::new())));
        assert_eq!(state.field_types[0], Some(Vec::new()));
    }
}
