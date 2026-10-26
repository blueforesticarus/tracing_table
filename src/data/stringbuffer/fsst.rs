//! Immutable FSST8 tables shared by a callsite's active and archived buffers.
//! SpiralDB trains and decodes symbols; the compact bucket encoder avoids its
//! 65,536-entry encoder lookup and training counters in retained storage.
use fsst_codec::{Compressor, Decompressor, Symbol};
use std::sync::Arc;

#[derive(Debug)]
pub(super) struct Table {
    symbols: [Symbol; 255],
    lengths: [u8; 255],
    // Only the small first-byte bucket is searched for each encoding step.
    buckets: [u16; 257],
    codes: [u8; 255],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_tables_round_trip_binary_unicode_and_short_tails() {
        // Embedded/trailing NULs and escape bytes exercise explicit symbol
        // lengths, rather than assuming that every stored string is plain ASCII.
        let binary: Vec<u8> = (0..=255).collect();
        let samples = [
            binary.as_slice(),
            b"render shader resource binding texture descriptor",
            "unseen Unicode é🦀".as_bytes(),
        ];
        let table = Table::train(&samples).unwrap();
        for length in 0..=binary.len() {
            let input = &binary[..length];
            assert_eq!(table.decode(&table.encode(input)).as_deref(), Some(input));
        }
        for input in [b"a\0b\0".as_slice(), b"\0\0\0", "é🦀".as_bytes()] {
            assert_eq!(table.decode(&table.encode(input)).as_deref(), Some(input));
        }
        assert_eq!(Table::allocation_bytes(), 3080);
    }

    #[test]
    fn malformed_codes_remain_fallible() {
        // The upstream decoder is infallible; reject malformed input before it.
        let table = Table::train(&[b"aaaaaaaaaaaaaaaa"]).unwrap();
        assert_eq!(table.decode(&[255]), None);
        let unused = table
            .lengths
            .iter()
            .position(|&length| length == 0)
            .unwrap();
        assert_eq!(table.decode(&[unused as u8]), None);
        assert_eq!(table.decode(&[255, 255]), Some(vec![255]));
        assert!(Table::train(&[]).is_none());
        assert!(Table::train(&[b""]).is_none());
    }
}

impl Table {
    pub fn train(texts: &[&[u8]]) -> Option<Arc<Self>> {
        if !texts.iter().any(|text| !text.is_empty()) {
            return None;
        }
        // Drop the upstream encoder's large transient lookup by retaining only
        // persistable parts. Tables remain one 3,080-byte Arc per callsite.
        let parts = Compressor::train(&texts.to_vec()).into_parts();
        let mut table = Self {
            symbols: parts.symbols,
            lengths: parts.lengths,
            buckets: [0; 257],
            codes: [0; 255],
        };
        // SpiralDB pads unused lengths with ones; only populated codes belong
        // in our encoder buckets or count as valid decoder input.
        table.lengths[usize::from(parts.n_symbols)..].fill(0);
        if table.lengths.iter().any(|&length| length > 8) {
            return None;
        }
        let mut codes: Vec<_> = (0..255u8)
            .filter(|&code| table.lengths[code as usize] > 0)
            .collect();
        codes.sort_by_key(|&code| {
            (
                table.symbols[code as usize].first_byte(),
                std::cmp::Reverse(table.lengths[code as usize]),
                code,
            )
        });
        table.codes[..codes.len()].copy_from_slice(&codes);
        let mut cursor = 0;
        for byte in 0..=256 {
            while cursor < codes.len()
                && usize::from(table.symbols[codes[cursor] as usize].first_byte()) < byte
            {
                cursor += 1;
            }
            table.buckets[byte] = cursor as u16;
        }
        Some(Arc::new(table))
    }

    pub fn encode(&self, text: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(text.len());
        let mut cursor = 0;
        while cursor < text.len() {
            let byte = usize::from(text[cursor]);
            let mut matched = false;
            for &code in &self.codes[self.buckets[byte] as usize..self.buckets[byte + 1] as usize] {
                let index = usize::from(code);
                let length = usize::from(self.lengths[index]);
                if text[cursor..].starts_with(&self.symbols[index].to_u64().to_le_bytes()[..length])
                {
                    output.push(code);
                    cursor += length;
                    matched = true;
                    break;
                }
            }
            if !matched {
                output.extend_from_slice(&[255, text[cursor]]);
                cursor += 1;
            }
        }
        output
    }

    pub fn decode(&self, codes: &[u8]) -> Option<Vec<u8>> {
        // Preserve our fallible API: the upstream decoder expects valid codes
        // and panics on a dangling escape. Check before entering that decoder.
        let mut cursor = 0;
        while cursor < codes.len() {
            let code = usize::from(codes[cursor]);
            cursor += 1;
            if code == 255 {
                codes.get(cursor)?;
                cursor += 1;
            } else {
                let length = usize::from(self.lengths[code]);
                if length == 0 {
                    return None;
                }
            }
        }
        Some(Decompressor::new(&self.symbols, &self.lengths).decompress(codes))
    }

    pub fn allocation_bytes() -> usize {
        // Arc stores its strong/weak counters alongside the one table allocation.
        size_of::<Self>() + 2 * size_of::<usize>()
    }
}
