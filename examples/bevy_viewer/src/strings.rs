//! Owned string diagnostics. Refresh and sorting happen outside egui row rendering.
use tracing_table::data::{
    dataframe::CompressionStats,
    stringcache::{Atom, AtomIndexed, Index, Stats},
};

pub struct StringEntry {
    pub index: usize,
    pub count: usize,
    pub text: Atom,
}

#[derive(Default)]
pub struct StringSnapshot {
    pub entries: Vec<StringEntry>,
    pub max_index: Option<usize>,
    pub index_bytes: usize,
    pub atoms: CompressionStats,
    pub forward: CompressionStats,
    pub reverse: CompressionStats,
    pub tracked: usize,
    pub constructions: usize,
    pub unique_bytes: usize,
    pub handle_bytes: usize,
    pub min_bytes: usize,
    pub max_bytes: usize,
    pub estimated_forward_capacity: usize,
}
impl StringSnapshot {
    pub fn capture(stats: &mut Stats, index: &Index) -> Self {
        // The main crate stays untouched. Its public API resolves sequential IDs;
        // Index::insert assigns IDs without gaps, so the first missing ID ends the list.
        let mut entries = Vec::new();
        while let Some(atom) = index.get(AtomIndexed::from_index(entries.len())) {
            entries.push(StringEntry {
                index: entries.len(),
                count: stats.get(&atom),
                text: atom,
            });
        }
        let max_index = entries.last().map(|entry| entry.index);
        // IDs are tagged by shifting left one bit; upper zero bytes should compact.
        let index_bytes = max_index.map_or(0, |id| {
            ((usize::BITS - (id << 1).leading_zeros()).max(1) as usize).div_ceil(8)
        });
        let forward = index.stats();
        // The public index stats cover the forward map only. Estimate occupied
        // reverse buckets with the same layout; its spare capacity is unavailable.
        let reverse = CompressionStats {
            overhead: forward.overhead,
            ..Default::default()
        };
        let bucket = (size_of::<(Atom, usize)>() as f64 + 1.0) / 0.875;
        let estimated_forward_capacity =
            entries.len() + (forward.overalloc as f64 / bucket).round() as usize;
        let tracked = stats.interned_count();
        let constructions = entries.iter().map(|entry| entry.count).sum::<usize>();
        let unique_bytes = entries
            .iter()
            .filter(|entry| entry.count > 0)
            .map(|entry| entry.text.len())
            .sum();
        let min_bytes = entries
            .iter()
            .filter(|entry| entry.count > 0)
            .map(|entry| entry.text.len())
            .min()
            .unwrap_or(0);
        let max_bytes = entries
            .iter()
            .filter(|entry| entry.count > 0)
            .map(|entry| entry.text.len())
            .max()
            .unwrap_or(0);
        Self {
            entries,
            max_index,
            index_bytes,
            atoms: stats.stats(),
            forward,
            reverse,
            tracked,
            constructions,
            unique_bytes,
            handle_bytes: constructions * size_of::<Atom>(),
            min_bytes,
            max_bytes,
            estimated_forward_capacity,
        }
    }
    pub fn total(&self) -> CompressionStats {
        self.atoms + self.forward + self.reverse
    }
    pub fn sort(&mut self, column: StringSortColumn, descending: bool) {
        self.entries.sort_unstable_by(|a, b| {
            let order = match column {
                StringSortColumn::Index => a.index.cmp(&b.index),
                StringSortColumn::Count => a.count.cmp(&b.count),
            };
            // Stable index tie-breaking keeps equally popular strings from jumping.
            (if descending { order.reverse() } else { order }).then_with(|| a.index.cmp(&b.index))
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StringSortColumn {
    Index,
    Count,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_accounts_for_text_handles_and_both_maps() {
        let mut stats = Stats::default();
        let mut index = Index::default();
        for text in [
            "long string alpha",
            "long string beta",
            "long string alpha",
            "short",
        ] {
            let atom = Atom::from(text);
            stats.inc(&atom);
            index.insert(atom);
        }
        let snapshot = StringSnapshot::capture(&mut stats, &index);
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.tracked, 2);
        assert_eq!(snapshot.constructions, 3);
        assert_eq!(
            snapshot.unique_bytes,
            "long string alpha".len() + "long string beta".len()
        );
        assert_eq!(snapshot.handle_bytes, 3 * size_of::<Atom>());
        assert_eq!(
            snapshot.atoms.compressed,
            snapshot.unique_bytes + snapshot.handle_bytes
        );
        assert_eq!(
            snapshot.total().overhead,
            snapshot.atoms.overhead + snapshot.forward.overhead + snapshot.reverse.overhead
        );
        assert_eq!(index.stats().overhead, snapshot.forward.overhead);
        assert!(snapshot.forward.overhead > 0 && snapshot.reverse.overhead > 0);
        assert!(snapshot.entries.iter().all(|entry| entry.text.len() > 7));
    }

    #[test]
    fn sorting_reverses_and_breaks_equal_counts_by_index() {
        let mut stats = Stats::default();
        let mut index = Index::default();
        for text in [
            "long string alpha",
            "long string beta",
            "long string beta",
            "long string gamma",
        ] {
            let atom = Atom::from(text);
            stats.inc(&atom);
            index.insert(atom);
        }
        let mut snapshot = StringSnapshot::capture(&mut stats, &index);
        snapshot.sort(StringSortColumn::Index, false);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.index)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        snapshot.sort(StringSortColumn::Index, true);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.index)
                .collect::<Vec<_>>(),
            [2, 1, 0]
        );
        snapshot.sort(StringSortColumn::Count, true);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.index)
                .collect::<Vec<_>>(),
            [1, 0, 2]
        );
        snapshot.sort(StringSortColumn::Count, false);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.index)
                .collect::<Vec<_>>(),
            [0, 2, 1]
        );
        let empty = StringSnapshot::capture(&mut Stats::default(), &Index::default());
        assert!(empty.entries.is_empty());
        assert_eq!(
            (
                empty.constructions,
                empty.unique_bytes,
                empty.min_bytes,
                empty.max_bytes
            ),
            (0, 0, 0, 0)
        );
    }
    #[test]
    fn inline_suffix_outlier_keeps_all_eight_bytes_in_a_block() {
        use tracing_table::data::{
            dataframe::Frame,
            dyntuple::{BytePacker, ValueData},
            stringcache::AtomIndexed,
        };
        fn varying_bytes(values: impl IntoIterator<Item = AtomIndexed>) -> usize {
            let width = size_of::<AtomIndexed>();
            let mut frame = Frame::new(width, 8);
            for handle in values {
                let (bytes, row) = frame.allocate().unwrap();
                BytePacker::new(bytes).write_any_value(&ValueData::StrIndex(handle), false);
                frame.compute_mask(row);
            }
            width - frame.mask().constant_bytes()
        }
        // Tagged IDs up to 300k only vary in the low three bytes.
        assert_eq!(
            varying_bytes([
                AtomIndexed::from_index(100_000),
                AtomIndexed::from_index(300_000)
            ]),
            3
        );
        // A seven-byte suffix is inline text, not an ID. One such row marks all
        // positions varying, even if every later suffix is a small indexed ID.
        assert_eq!(
            varying_bytes([
                AtomIndexed::from_small_str("1234567"),
                AtomIndexed::from_index(300_000)
            ]),
            8
        );
    }
}
