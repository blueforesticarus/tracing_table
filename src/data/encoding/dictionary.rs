//! Packed dictionaries with optional flat key prediction. Prediction arithmetic
//! is shared through helpers, rather than another Encoding inside this encoding.
use super::common::*;
use super::{Context, Encoding, Footer, Layout};
use crate::data::util::float_u16::FloatU16;
use smallvec::SmallVec;

#[derive(Clone, Copy)]
pub struct DictionaryEncoding<'a> {
    entries: &'a [u8],
    count: usize,
    bits: usize,
    prediction: KeyPrediction,
}

// Key prediction always starts at zero, preserving the first slot's dictionary base.
#[derive(Clone, Copy)]
pub(crate) enum KeyPrediction {
    Raw,
    Cycle(u8, u8),
    Stride(u64),
}
impl KeyPrediction {
    fn at(self, row: usize, length: usize) -> u128 {
        match self {
            Self::Raw => 0,
            Self::Cycle(period, phase) => key_prediction(row, period, phase),
            Self::Stride(stride) => u128::from(advance(stride, row, length)),
        }
    }
    fn encode(self, id: u128, row: usize, length: usize, width: usize) -> u128 {
        if matches!(self, Self::Raw) {
            id
        } else {
            residual(id, self.at(row, length), width)
        }
    }
    fn decode(self, encoded: u128, row: usize, length: usize, width: usize) -> u128 {
        if matches!(self, Self::Raw) {
            encoded
        } else {
            restore(encoded, self.at(row, length), width)
        }
    }
}

impl<'a> Encoding<'a> for DictionaryEncoding<'a> {
    fn parse(record: &'a [u8], layout: Layout, _rows: usize) -> Self {
        let width = usize::from(layout.width);
        let count = usize::from(record[1]);
        let bits = usize::from(record[2]);
        assert!((2..=MAX_DICTIONARY_VALUES).contains(&count));
        assert!((1..=width * 8).contains(&bits));
        let (prediction, header) = match record[0] {
            DICTIONARY => (KeyPrediction::Raw, 3),
            DICTIONARY_CYCLE => {
                assert!(
                    record[3] != 0 && record[4] < record[3],
                    "invalid dictionary cycle"
                );
                (KeyPrediction::Cycle(record[3], record[4]), 5)
            }
            DICTIONARY_STRIDE => (
                KeyPrediction::Stride(
                    FloatU16::from_bits(u16::from_le_bytes(record[3..5].try_into().unwrap()))
                        .to_u64(),
                ),
                5,
            ),
            _ => panic!("invalid dictionary tag"),
        };
        let end = header + (count * bits).div_ceil(8);
        check_record_size(record, end);
        Self {
            entries: &record[header..end],
            count,
            bits,
            prediction,
        }
    }
    fn write_footer(&self, _width: usize, output: &mut Footer) {
        let tag = match self.prediction {
            KeyPrediction::Raw => DICTIONARY,
            KeyPrediction::Cycle(..) => DICTIONARY_CYCLE,
            KeyPrediction::Stride(_) => DICTIONARY_STRIDE,
        };
        output.extend_from_slice(&[tag, self.count as u8, self.bits as u8]);
        match self.prediction {
            KeyPrediction::Raw => {}
            KeyPrediction::Cycle(period, phase) => output.extend_from_slice(&[period, phase]),
            KeyPrediction::Stride(stride) => {
                output.extend_from_slice(&FloatU16::from_u64_lossy(stride).to_bits().to_le_bytes())
            }
        }
        output.extend_from_slice(self.entries);
    }
    #[inline]
    fn decode(&self, encoded: u128, row: usize, context: Context) -> u128 {
        let id = self.prediction.decode(
            encoded ^ context.first(),
            row,
            context.rows(),
            context.width(),
        ) as usize;
        assert!(id < self.count, "dictionary key exceeds entry count");
        context
            .first()
            .checked_add(unpack(self.entries, id, self.bits))
            .expect("dictionary value exceeds u128")
    }
    #[inline]
    fn encode(&self, value: u128, row: usize, context: Context) -> Option<u128> {
        let delta = value.checked_sub(context.first())?;
        let id = (0..self.count).find(|&id| unpack(self.entries, id, self.bits) == delta)?;
        if row == 0 && id != 0 {
            return None;
        }
        Some(
            context.first()
                ^ self
                    .prediction
                    .encode(id as u128, row, context.rows(), context.width()),
        )
    }
}

fn key_prediction(row: usize, period: u8, phase: u8) -> u128 {
    (((row % usize::from(period) + usize::from(phase)) % usize::from(period)) as u128)
        .wrapping_sub(u128::from(phase))
}
/// Dictionary economics are deliberately explicit. First/mask slots and the
/// one-byte directory entry are shared; the minimum reuses the first slot.
pub(crate) fn dictionary_cost(unique: &[u128], cost: &Cost<'_>) -> (usize, usize) {
    let minimum = *unique.iter().min().unwrap();
    let entry_mask = unique
        .iter()
        .fold(0, |mask, &value| mask | (value - minimum));
    let entry_bits = (u128::BITS - entry_mask.leading_zeros()) as usize;
    let key_bits = (usize::BITS - (unique.len() - 1).leading_zeros()) as usize;
    let key_bits = if cfg!(feature = "bit-compression") {
        key_bits
    } else {
        key_bits.div_ceil(8) * 8
    };
    let metadata_bytes = 3 + (unique.len() * entry_bits).div_ceil(8);
    // Every discovery-order key in 0..K occurs, so the direct mask is exact.
    // Predicted keys require inspecting their residuals separately.
    (cost.size(key_bits, metadata_bytes), entry_bits)
}

pub(crate) fn select_dictionary(best: &mut Candidate, values: &[u128], cost: &Cost<'_>) {
    // The smallest dictionary has a tag, count, width and at least one entry byte.
    if !best.worthwhile(cost.size(0, 4), cost) {
        return;
    }
    let width = cost.width;
    let mut unique = SmallVec::<[u128; 64]>::new();
    let mut ids = SmallVec::<[u128; 256]>::new();
    for &value in values {
        let id = match unique.iter().position(|&entry| entry == value) {
            Some(id) => id,
            None if unique.len() < MAX_DICTIONARY_VALUES => {
                unique.push(value);
                unique.len() - 1
            }
            _ => return,
        };
        ids.push(id as u128);
    }
    if unique.len() < 2 {
        return;
    }
    let (direct_cost, entry_bits) = dictionary_cost(&unique, cost);
    let record_bytes = 3 + (unique.len() * entry_bits).div_ceil(8);
    // Even perfect key prediction must pay for the packed entries and cycle bytes.
    if !best.worthwhile(direct_cost, cost) && !best.worthwhile(cost.size(0, record_bytes + 2), cost)
    {
        return;
    }
    if record_bytes > MAX_RECORD_BYTES {
        return;
    }
    let base = *unique.iter().min().unwrap();
    let mut dictionary = SmallVec::<[u8; 24]>::new();
    dictionary.extend_from_slice(&[DICTIONARY, unique.len() as u8, entry_bits as u8]);
    append_packed(
        &mut dictionary,
        unique.iter().map(|&value| value - base),
        unique.len(),
        entry_bits,
    );
    debug_assert_eq!(dictionary.len(), record_bytes);
    if best.worthwhile(direct_cost, cost) {
        // Discovery assigns every key in 0..K. Their union mask and packed size
        // are known exactly, so direct dictionaries need no residual trial scan.
        let key_bits = (usize::BITS - (unique.len() - 1).leading_zeros()) as usize;
        best.size = direct_cost;
        best.mask = (1u128 << key_bits) - 1;
        best.record = dictionary.clone();
        best.encoded.clear();
        best.encoded.extend(ids.iter().map(|id| base ^ id));
    }
    // Stride and cycle keys have the same minimum metadata cost.
    if !best.worthwhile(cost.size(0, record_bytes + 2), cost) {
        return;
    }
    // Repeated runs of progressively discovered values benefit from a fractional
    // key stride. Charge its two-byte coefficient before considering cyclic keys.
    let mut predicted = dictionary.clone();
    predicted[0] = DICTIONARY_STRIDE;
    predicted.insert(3, 0);
    predicted.insert(4, 0);
    // Use one fitted key line, ending at the last occurrence of the highest
    // discovery key. A final revisit to an older key must not flatten the line.
    let endpoint = ids.iter().enumerate().max_by_key(|(_, id)| *id).unwrap().0;
    let stride =
        FloatU16::from_u64_lossy((ids[endpoint] * ids.len() as u128 / endpoint as u128) as u64);
    if stride.to_u64() != 0 {
        predicted[3..5].copy_from_slice(&stride.to_bits().to_le_bytes());
        let prediction = KeyPrediction::Stride(stride.to_u64());
        best.consider(&predicted, &ids, cost, |id, row| {
            base ^ prediction.encode(id, row, ids.len(), width)
        });
    }
    // IDs start at zero; cyclic prediction therefore needs no stored origin.
    let first_return = ids
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(row, &id)| (id == 0).then_some(row));
    dictionary[0] = DICTIONARY_CYCLE;
    dictionary.insert(3, 0);
    dictionary.insert(4, 0);
    if !best.worthwhile(cost.size(0, dictionary.len()), cost) {
        return;
    }
    for period in cycle_periods(&ids) {
        let rotated = first_return.map_or(0, |row| {
            ((usize::from(period) - row % usize::from(period)) % usize::from(period)) as u8
        });
        for (choice, phase) in [0, rotated].into_iter().enumerate() {
            if choice == 1 && phase == 0 {
                continue;
            }
            dictionary[3] = period;
            dictionary[4] = phase;
            let previous = best.size;
            best.consider(&dictionary, &ids, cost, |id, row| {
                base ^ KeyPrediction::Cycle(period, phase).encode(id, row, ids.len(), width)
            });
            if best.size < previous {
                return;
            }
        }
    }
}
