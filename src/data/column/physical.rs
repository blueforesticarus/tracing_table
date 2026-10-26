//! Archetype-owned physical layouts. History boundaries are absolute block
//! ordinals, so eviction and promotion never change row IDs or archived bytes.
//! Signed payloads use ZigZag before optional column codecs; logical rows and
//! portable input dumps retain their original signed values and native types.
use super::{Layout, read_word, residual, restore, write_word};
use smallvec::SmallVec;
use std::ops::Range;

/// Useful for exact replay ablations; normal recording enables all features.
#[derive(Clone, Copy, Debug)]
pub struct LayoutPolicy {
    pub sharing: bool,
    pub integers: bool,
    pub floats: bool,
}
impl Default for LayoutPolicy {
    fn default() -> Self {
        Self {
            sharing: true,
            integers: true,
            floats: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub logical: Layout,
    pub stored: Layout,
    pub source: u16,
    pub family: u8,
    pub signed: bool,
    pub integer: bool,
    // Only logical f64 fields convert; native f32 fields retain their own family.
    pub float64: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalLayout {
    pub block_index: usize,
    // Do not reserve eight fields inside every archetype/history entry. The
    // outer history, rather than these variable-sized maps, has inline room.
    pub fields: Vec<Field>,
    pub columns: Vec<Layout>,
    // Compact schema dimensions use the same u16 bound as column offsets.
    header: u16,
    unit: u16,
    logical_unit: u16,
    duration: bool,
    sharing: bool,
}

impl PhysicalLayout {
    /// Definitions come from the logical schema once, never from row contents.
    pub fn new(
        header: usize,
        logical_unit: usize,
        duration: bool,
        mut fields: SmallVec<[Field; 8]>,
        policy: LayoutPolicy,
    ) -> Self {
        for field in &mut fields {
            if field.integer && policy.integers {
                field.stored.width = field.logical.width.min(2);
            } else if field.float64 && policy.floats {
                field.stored.width = 4;
            }
        }
        let mut layout = Self {
            block_index: 0,
            fields: fields.into_vec(),
            columns: Vec::new(),
            header: header.try_into().expect("row header exceeds u16"),
            unit: header.try_into().expect("row header exceeds u16"),
            logical_unit: logical_unit.try_into().expect("logical row exceeds u16"),
            duration,
            sharing: policy.sharing,
        };
        layout.positions();
        layout
    }

    pub fn header(&self) -> usize {
        usize::from(self.header)
    }

    pub fn unit(&self) -> usize {
        usize::from(self.unit)
    }

    pub fn logical_unit(&self) -> usize {
        usize::from(self.logical_unit)
    }

    fn positions(&mut self) {
        self.unit = self.header;
        self.columns.clear();
        if self.duration {
            self.columns.push(Layout::new(8, 8, false));
        }
        for i in 0..self.fields.len() {
            let source = usize::from(self.fields[i].source);
            if source == i {
                self.fields[i].stored.offset = self.unit;
                self.unit = self
                    .unit
                    .checked_add(u16::from(self.fields[i].stored.width))
                    .expect("physical row exceeds u16");
                self.columns.push(self.fields[i].stored);
            } else {
                assert!(source < i);
                self.fields[i].stored = self.fields[source].stored;
            }
        }
    }

    fn required(field: &Field, value: u128) -> u8 {
        if field.float64 {
            let value = f64::from_bits(value as u64);
            // Compare bits, including signed zero and NaN payloads. A failed
            // conversion promotes the active block through the integer path.
            return if ((value as f32) as f64).to_bits() == value.to_bits() {
                4
            } else {
                8
            };
        }
        if !field.integer {
            return field.logical.width;
        }
        for width in [2u8, 4, 8, 16] {
            if width >= field.logical.width {
                return field.logical.width;
            }
            let bits = u32::from(width) * 8;
            let fits = if field.signed {
                let logical_bits = u32::from(field.logical.width) * 8;
                let signed = ((value << (128 - logical_bits)) as i128) >> (128 - logical_bits);
                signed >= -(1i128 << (bits - 1)) && signed < (1i128 << (bits - 1))
            } else {
                value < (1u128 << bits)
            };
            if fits {
                return width;
            }
        }
        unreachable!()
    }

    /// Equality groups only split. A new canonical column inherits the old
    /// group's prefix, including when several aliases diverge together.
    pub fn promoted(&self, row: &[u8], first: bool) -> Option<Self> {
        let first = first && self.sharing;
        let value = |i: usize| read_word(&row[self.fields[i].logical.range()]);
        let mut changed = false;
        for (i, f) in self.fields.iter().enumerate() {
            changed |= Self::required(f, value(i)) > f.stored.width;
            changed |= usize::from(f.source) != i && value(i) != value(usize::from(f.source));
            if first {
                changed |=
                    (0..i).any(|j| self.fields[j].family == f.family && value(i) == value(j));
            }
        }
        if !changed {
            return None;
        }
        let mut next = self.clone();
        for i in 0..next.fields.len() {
            let old_source = self.fields[i].source;
            let source = (0..i)
                .find(|&j| {
                    (first || self.fields[j].source == old_source)
                        && self.fields[j].family == self.fields[i].family
                        && value(j) == value(i)
                })
                .map(|j| next.fields[j].source)
                .unwrap_or_else(|| i.try_into().expect("too many columns"));
            next.fields[i].source = source;
            next.fields[i].stored.width = self.fields[i]
                .stored
                .width
                .max(Self::required(&self.fields[i], value(i)));
        }
        // The widest member determines storage when a group shares a column.
        for i in (0..next.fields.len()).rev() {
            let source = usize::from(next.fields[i].source);
            next.fields[source].stored.width = next.fields[source]
                .stored
                .width
                .max(next.fields[i].stored.width);
        }
        next.positions();
        Some(next)
    }

    pub fn pack(&self, logical: &[u8], physical: &mut [u8]) {
        assert_eq!(logical.len(), self.logical_unit());
        assert_eq!(physical.len(), self.unit());
        physical[..self.header()].copy_from_slice(&logical[..self.header()]);
        for (i, field) in self.fields.iter().enumerate() {
            let value = read_word(&logical[field.logical.range()]);
            if usize::from(field.source) == i {
                assert!(
                    Self::required(field, value) <= field.stored.width,
                    "field needs promotion"
                );
                // Signed payloads always enter active/archived storage as ZigZag.
                // Apply it at logical width before narrowing so negative extrema
                // retain their sign; optional column codecs see unsigned words.
                let value = if field.float64 && field.stored.width == 4 {
                    u128::from((f64::from_bits(value as u64) as f32).to_bits())
                } else if field.signed {
                    residual(value, 0, usize::from(field.logical.width))
                } else {
                    value
                };
                write_word(&mut physical[field.stored.range()], value);
            } else {
                assert_eq!(
                    value,
                    read_word(&logical[self.fields[usize::from(field.source)].logical.range()]),
                    "alias needs promotion"
                );
            }
        }
    }

    pub fn unpack(&self, physical: &[u8], logical: &mut [u8]) {
        assert_eq!(logical.len(), self.logical_unit());
        logical[..self.header()].copy_from_slice(&physical[..self.header()]);
        for field in &self.fields {
            let mut value = read_word(&physical[field.stored.range()]);
            if field.float64 && field.stored.width == 4 {
                // Logical rows always expose the original f64 type and bits;
                // archived codecs operate only on the narrower physical word.
                value = u128::from((f32::from_bits(value as u32) as f64).to_bits());
            }
            let bits = u32::from(field.stored.width) * 8;
            // Restore at stored width, then sign-extend to the logical type.
            // This inverse also covers aliases, promotions, and archived edits.
            if field.signed {
                value = restore(value, 0, usize::from(field.stored.width));
            }
            if field.signed && bits < 128 && value & (1u128 << (bits - 1)) != 0 {
                value |= u128::MAX << bits;
            }
            write_word(&mut logical[field.logical.range()], value);
        }
    }

    /// None denotes an alias: its physical bytes belong to its canonical field.
    pub fn physical_range(&self, logical: Range<usize>) -> Option<Range<usize>> {
        if logical.end <= self.header() {
            return Some(logical);
        }
        self.fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.logical.range() == logical)
            .and_then(|(i, f)| (usize::from(f.source) == i).then(|| f.stored.range()))
    }

    pub fn heap_bytes(&self) -> usize {
        self.fields.capacity() * size_of::<Field>() + self.columns.capacity() * size_of::<Layout>()
    }
}

#[cfg(test)]
mod tests;
