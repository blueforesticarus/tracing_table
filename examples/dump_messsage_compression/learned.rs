//! Per-archetype learning shared by the block inspectors.

use std::collections::HashMap;

use super::{Algorithm, Rendering, Row, escape_template_literal, render_algorithm};

const MAX_PATTERNS: usize = 128;
const MAX_SEEDS: usize = 16;

#[cfg(test)]
pub(super) fn test_pattern_captures(template: &str, row: &Row) -> Option<Vec<String>> {
    Pattern::new(template.to_owned()).captures(row)
}

pub(super) fn capture_types(template: &str, row: &Row, fields: &[String]) -> Vec<super::ValueType> {
    // Use original capture offsets, not a heuristic on the field's contents.
    // A gap containing a mask plus punctuation or other text remains a string.
    let pattern = Pattern::new(template.to_owned());
    let mut cursor = pattern.literals[0].len();
    fields
        .iter()
        .enumerate()
        .map(|(slot, field)| {
            let end = cursor + field.len();
            let index = row.tokens.partition_point(|token| token.end <= cursor);
            let kind = row
                .tokens
                .get(index)
                .filter(|token| token.start == cursor && token.end == end)
                .and_then(|_| row.masked.get(index).copied().flatten())
                .unwrap_or(super::ValueType::String);
            cursor = end + pattern.literals[slot + 1].len();
            kind
        })
        .collect()
}

struct Pattern {
    template: String,
    // Decoded literal pieces on either side of each {} value slot.
    literals: Vec<String>,
    static_bytes: usize,
    // Preextracted values stay variable even when a later block repeats one
    // value. Other slots retain the existing constant-promotion behavior.
    forced_slots: Vec<bool>,
}

impl Pattern {
    fn new(template: String) -> Self {
        let mut literals = vec![String::new()];
        let mut chars = template.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                let decoded = match chars.next() {
                    Some('t') => '\t',
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some(other) => other,
                    None => '\\',
                };
                literals.last_mut().unwrap().push(decoded);
            } else if ch == '{' && chars.peek() == Some(&'}') {
                chars.next();
                literals.push(String::new());
            } else {
                literals.last_mut().unwrap().push(ch);
            }
        }
        let static_bytes = literals.iter().map(String::len).sum();
        Self {
            template,
            forced_slots: vec![false; literals.len() - 1],
            literals,
            static_bytes,
        }
    }

    fn captures(&self, row: &Row) -> Option<Vec<String>> {
        let message = &row.message;
        let first = &self.literals[0];
        if !message.starts_with(first) {
            return None;
        }
        let mut cursor = first.len();
        let mut fields = Vec::with_capacity(self.literals.len() - 1);
        for (i, literal) in self.literals.iter().enumerate().skip(1) {
            let last = i + 1 == self.literals.len();
            let at = if last {
                let at = message.len().checked_sub(literal.len())?;
                if at < cursor || !message[at..].starts_with(literal) {
                    return None;
                }
                at
            } else {
                cursor + message[cursor..].find(literal)?
            };
            if row.inside_mask(cursor) || row.inside_mask(at) {
                return None;
            }
            fields.push(message[cursor..at].to_owned());
            cursor = at + literal.len();
        }
        (cursor == message.len()).then_some(fields)
    }
}

#[derive(Default)]
pub(super) struct LearnedState {
    // Drain2 carries its typed regex clusters, independently of the generic
    // literal-search cache used by the other exploratory algorithms.
    drain2: super::drain2::State,
    patterns: Vec<Pattern>,
    exact: HashMap<String, usize>,
    // Unmatched examples can form a template with a later block's message.
    seeds: Vec<Row>,
}

impl LearnedState {
    fn add_pattern(&mut self, template: String, row: &Row, fields: &[String]) {
        let index = if let Some(index) = self.patterns.iter().position(|p| p.template == template) {
            index
        } else {
            if self.patterns.len() == MAX_PATTERNS {
                self.patterns.remove(0);
                self.exact.clear(); // IDs shift when the oldest pattern leaves.
            }
            let index = self.patterns.len();
            self.patterns.push(Pattern::new(template));
            index
        };
        let pattern = &mut self.patterns[index];
        let mut cursor = pattern.literals[0].len();
        for (slot, field) in fields.iter().enumerate() {
            pattern.forced_slots[slot] |= row.contains_mask(cursor, cursor + field.len());
            cursor += field.len() + pattern.literals[slot + 1].len();
        }
        if self.exact.len() >= 4096 {
            self.exact.clear();
        }
        self.exact.insert(row.message.clone(), index);
    }

    fn matched_pattern(&self, row: &Row) -> Option<(usize, Vec<String>)> {
        if let Some(&index) = self.exact.get(&row.message) {
            if let Some(fields) = self.patterns[index].captures(row) {
                return Some((index, fields));
            }
        }
        self.patterns
            .iter()
            .enumerate()
            .filter_map(|(index, pattern)| {
                pattern
                    .captures(row)
                    .map(|fields| (index, pattern.static_bytes, fields))
            })
            .max_by_key(|(_, bytes, _)| *bytes)
            .map(|(index, _, fields)| (index, fields))
    }

    fn refine(
        &self,
        pattern_index: usize,
        matches: &[(usize, Vec<String>)],
    ) -> Vec<(usize, Rendering)> {
        let pattern = &self.patterns[pattern_index];
        let mut template = escape_template_literal(&pattern.literals[0]);
        let mut fields = vec![Vec::new(); matches.len()];
        for slot in 0..pattern.literals.len() - 1 {
            // Keep the reusable cached template for one-row blocks. When a
            // current block has repeated matches, promote constant values.
            let constant = !pattern.forced_slots[slot]
                && matches.len() >= 2
                && matches
                    .iter()
                    .all(|(_, values)| values[slot] == matches[0].1[slot]);
            if constant {
                template.push_str(&escape_template_literal(&matches[0].1[slot]));
            } else {
                template.push_str("{}");
                for (out, (_, values)) in fields.iter_mut().zip(matches) {
                    out.push(values[slot].clone());
                }
            }
            template.push_str(&escape_template_literal(&pattern.literals[slot + 1]));
        }
        matches
            .iter()
            .enumerate()
            .map(|(i, (row, _))| (*row, (template.clone(), fields[i].clone())))
            .collect()
    }

    fn compatible_seed(algorithm: Algorithm, seed: &Row, row: &Row) -> bool {
        match algorithm {
            Algorithm::Drain => {
                seed.tokens.len() == row.tokens.len()
                    && seed
                        .tokens
                        .first()
                        .zip(row.tokens.first())
                        .is_some_and(|_| seed.token(0) == row.token(0))
            }
            Algorithm::Drain2 => false, // Uses its own regex-cluster carry state.
            Algorithm::Spell => seed
                .tokens
                .first()
                .zip(row.tokens.first())
                .is_some_and(|_| seed.token(0) == row.token(0)),
            Algorithm::Mdl | Algorithm::Mdl2 => seed.tokens.len() == row.tokens.len(),
            Algorithm::Msa | Algorithm::Sequitur => seed.tokens.iter().any(|token| {
                let word = &seed.message[token.start..token.end];
                word.len() > 1
                    && row
                        .tokens
                        .iter()
                        .any(|other| row.message[other.start..other.end] == *word)
            }),
            Algorithm::Prefix => true,
            Algorithm::None => false,
        }
    }

    pub(super) fn render_block(
        &mut self,
        rows: &[Row],
        algorithm: Algorithm,
    ) -> Vec<Option<Rendering>> {
        if matches!(algorithm, Algorithm::Drain2) {
            return self.drain2.render_block(rows);
        }
        if matches!(algorithm, Algorithm::None) {
            return rows
                .iter()
                .map(|row| Some(("{}".to_owned(), vec![row.message.clone()])))
                .collect();
        }
        let mut rendered = vec![None; rows.len()];
        let mut cached = HashMap::<usize, Vec<(usize, Vec<String>)>>::new();
        for (index, row) in rows.iter().enumerate() {
            if let Some((pattern, fields)) = self.matched_pattern(row) {
                cached.entry(pattern).or_default().push((index, fields));
            }
        }
        for (pattern, matches) in cached {
            for (index, output) in self.refine(pattern, &matches) {
                rendered[index] = Some(output);
            }
        }

        let unknown: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(i, _)| rendered[*i].is_none())
            .map(|(i, row)| (i, row.clone()))
            .collect();
        if unknown.len() >= 2 {
            let subset: Vec<_> = unknown.iter().map(|(_, row)| row.clone()).collect();
            let analyzed = if matches!(algorithm, Algorithm::Prefix) {
                super::prefix_render(&subset)
            } else {
                render_algorithm(&subset, algorithm)
            };
            for ((index, row), result) in unknown.iter().zip(analyzed) {
                if let Some((template, fields)) = result {
                    self.add_pattern(template.clone(), row, &fields);
                    rendered[*index] = Some((template, fields));
                }
            }
        }

        // Try a few prior unmatched examples only for rows still unexplained.
        for (index, row) in rows.iter().enumerate() {
            if rendered[index].is_some() {
                continue;
            }
            let mut learned = None;
            for seed in self
                .seeds
                .iter()
                .rev()
                .filter(|seed| Self::compatible_seed(algorithm, seed, row))
                .take(4)
            {
                let pair = [seed.clone(), row.clone()];
                let result = if matches!(algorithm, Algorithm::Prefix) {
                    super::prefix_render(&pair)
                } else {
                    render_algorithm(&pair, algorithm)
                };
                if let Some(output) = result.into_iter().nth(1).flatten() {
                    learned = Some(output);
                    break;
                }
            }
            if let Some((template, fields)) = learned {
                self.add_pattern(template.clone(), row, &fields);
                rendered[index] = Some((template, fields));
            }
        }
        for (row, result) in rows.iter().zip(&rendered) {
            if result.is_none() && !self.seeds.iter().any(|seed| seed.message == row.message) {
                if self.seeds.len() == MAX_SEEDS {
                    self.seeds.remove(0);
                }
                self.seeds.push(row.clone());
            }
        }
        rendered
    }

    pub(super) fn drain2_field_types(&self) -> &[Option<Vec<super::ValueType>>] {
        &self.drain2.field_types
    }
}
