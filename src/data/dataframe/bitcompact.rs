use std::{
    alloc::{self, Layout},
    mem::MaybeUninit,
    ptr, slice,
};

use streaming_iterator::StreamingIterator;

use super::{CompressionStats, Frame, Mask, NeedsRebuild, StackVec};

/// Stores the mask and first row once, then only varying bits for later rows.
/// Each packed row starts on a byte boundary and occupies ceil(varying_bits / 8)
/// bytes. Bits are ordered by source byte, then least-significant source bit.
#[repr(C, align(16))]
pub struct BitCompactFrame {
    unit: usize,
    length: usize,
    compact_unit: usize,
    // Private spare storage permits construction without zeroing the allocation.
    // new_from initializes every payload byte before exposing the completed frame.
    buffer: [MaybeUninit<u8>],
}

impl BitCompactFrame {
    pub fn new_from(frame: &Frame) -> Box<Self> {
        assert!(frame.unit > 0, "frame unit must be nonzero");
        let source_size = frame
            .length
            .checked_mul(frame.unit)
            .and_then(|size| size.checked_add(frame.unit))
            .expect("source frame size overflow");
        // Validate metadata before allocation; read only committed source rows.
        let source = &frame.buffer[..source_size];
        let mask = &source[..frame.unit];
        let varying_bits = mask
            .iter()
            .try_fold(0usize, |total, byte| {
                total.checked_add(byte.count_ones() as usize)
            })
            .expect("varying bit count overflow");
        let compact_unit = varying_bits.div_ceil(8);
        let header_size = frame
            .unit
            .checked_mul(if frame.length == 0 { 1 } else { 2 })
            .expect("bit compact frame header size overflow");
        let buffer_size = frame
            .length
            .saturating_sub(1)
            .checked_mul(compact_unit)
            .and_then(|size| size.checked_add(header_size))
            .expect("bit compact frame size overflow");
        let layout = allocation_layout(buffer_size);

        // SAFETY: Header fields are written before Box creation. The tail is
        // MaybeUninit, and Box's DST drop layout matches allocation_layout exactly.
        let mut compact = unsafe {
            let allocation = alloc::alloc(layout);
            if allocation.is_null() {
                alloc::handle_alloc_error(layout);
            }
            let raw: *mut Self = ptr::from_raw_parts_mut(allocation, buffer_size);
            ptr::addr_of_mut!((*raw).unit).write(frame.unit);
            ptr::addr_of_mut!((*raw).length).write(frame.length);
            ptr::addr_of_mut!((*raw).compact_unit).write(compact_unit);
            Box::from_raw(raw)
        };
        copy_bytes(&mut compact.buffer[..header_size], &source[..header_size]);

        for (index, row) in source[header_size..].chunks_exact(frame.unit).enumerate() {
            let offset = header_size + index * compact_unit;
            pack_row(
                row,
                mask,
                &mut compact.buffer[offset..offset + compact_unit],
            );
        }
        compact
    }

    pub fn unit(&self) -> usize {
        self.unit
    }
    pub fn len(&self) -> usize {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn compact_unit(&self) -> usize {
        self.compact_unit
    }

    /// Initialized mask, first row (if any), and packed rows; there is no spare tail.
    pub fn buffer(&self) -> &[u8] {
        // SAFETY: new_from writes the entire payload before returning it, and
        // private fields prevent callers from changing its initialization bounds.
        unsafe { slice::from_raw_parts(self.buffer.as_ptr().cast(), self.buffer.len()) }
    }

    pub fn mask(&self) -> &Mask {
        Mask::get(&self.buffer()[..self.unit])
    }

    pub fn first(&self) -> &[u8] {
        let end = if self.is_empty() {
            self.unit
        } else {
            2 * self.unit
        };
        &self.buffer()[self.unit..end]
    }

    /// Decode one row into an existing buffer, without allocating or clearing it.
    pub fn read_row(&self, index: usize, output: &mut [u8]) {
        assert!(index < self.length, "row index out of bounds");
        assert_eq!(output.len(), self.unit, "row size mismatch");
        output.copy_from_slice(self.first());
        self.unpack_varying(index, output);
    }

    /// Update in place, or return NeedsRebuild without changes if a constant bit changes.
    pub fn try_write_row(&mut self, index: usize, row: &[u8]) -> Result<(), NeedsRebuild> {
        assert!(index < self.length, "row index out of bounds");
        assert_eq!(row.len(), self.unit, "row size mismatch");
        let first = self.first();
        let constants_changed = self
            .mask()
            .iter()
            .enumerate()
            .any(|(i, &mask)| (row[i] ^ first[i]) & !mask != 0);
        if constants_changed {
            return Err(NeedsRebuild);
        }

        let unit = self.unit;
        let compact_unit = self.compact_unit;
        let (header, packed) = self.buffer.split_at_mut(2 * unit);
        if index == 0 {
            copy_bytes(&mut header[unit..], row);
        } else {
            // SAFETY: The mask is initialized and disjoint from the packed output.
            let mask = unsafe { slice::from_raw_parts(header.as_ptr().cast(), unit) };
            let offset = (index - 1) * compact_unit;
            pack_row(row, mask, &mut packed[offset..offset + compact_unit]);
        }
        Ok(())
    }

    fn unpack_varying(&self, index: usize, output: &mut [u8]) {
        if index == 0 {
            return;
        }
        let offset = 2 * self.unit + (index - 1) * self.compact_unit;
        let packed = &self.buffer()[offset..offset + self.compact_unit];
        let mut position = 0;
        for (value, &varying) in output.iter_mut().zip(self.mask().iter()) {
            *value &= !varying;
            let mut bits = varying;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                *value |= ((packed[position / 8] >> (position % 8)) & 1) << bit;
                position += 1;
                bits &= bits - 1;
            }
        }
    }

    pub fn iter(&self) -> BitCompactFrameIter<'_> {
        BitCompactFrameIter::new(self)
    }

    pub fn uncompact(&self, new: &mut Frame) {
        assert_eq!(new.unit, self.unit, "uncompact unit mismatch");
        assert!(
            new.capacity() >= self.length,
            "uncompact destination is too small"
        );
        new.length = 0;
        new.buffer[..self.unit].copy_from_slice(self.mask());
        for index in 0..self.length {
            let start = (index + 1) * self.unit;
            // SAFETY: Seed the entire destination row before creating a readable
            // slice over it. The source and destination are disjoint allocations.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.first().as_ptr(),
                    new.buffer.as_mut_ptr().add(start),
                    self.unit,
                );
            }
            self.unpack_varying(index, &mut new.buffer[start..start + self.unit]);
            // Publish after writing; the copied mask already describes these rows.
            new.length += 1;
        }
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            data: self.length * self.unit,
            compressed: self.buffer.len() - self.unit,
            overhead: size_of_val(self) - self.buffer.len() + self.unit,
            overalloc: 0,
        }
    }
}

// Use the same packing order for construction and in-place row updates.
fn pack_row(row: &[u8], mask: &[u8], output: &mut [MaybeUninit<u8>]) {
    let mut offset = 0;
    let mut packed = 0u8;
    let mut used = 0;
    for (&value, &varying) in row.iter().zip(mask) {
        let mut bits = varying;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            packed |= ((value >> bit) & 1) << used;
            used += 1;
            if used == 8 {
                output[offset].write(packed);
                offset += 1;
                packed = 0;
                used = 0;
            }
            bits &= bits - 1;
        }
    }
    if used != 0 {
        // Padding bits of the final byte stay zero on every update.
        output[offset].write(packed);
        offset += 1;
    }
    debug_assert_eq!(offset, output.len());
}

fn allocation_layout(buffer_size: usize) -> Layout {
    // Keep the three usize fields and trailing alignment padding in the DST layout.
    let size = size_of::<[usize; 3]>()
        .checked_add(buffer_size)
        .expect("bit compact frame allocation size overflow");
    Layout::from_size_align(size, 16)
        .expect("bit compact frame allocation exceeds layout limits")
        .pad_to_align()
}

fn copy_bytes(destination: &mut [MaybeUninit<u8>], source: &[u8]) {
    assert_eq!(destination.len(), source.len());
    // SAFETY: Both slices cover the checked byte count and cannot overlap.
    // A raw copy initializes the destination without reading its previous bytes.
    unsafe {
        ptr::copy_nonoverlapping(
            source.as_ptr(),
            destination.as_mut_ptr().cast(),
            source.len(),
        );
    }
}

pub struct BitCompactFrameIter<'a> {
    frame: &'a BitCompactFrame,
    // Match CompactFrameIter's alignment for callers interpreting decoded rows.
    buffer: StackVec<128, u128>,
    yielded: usize,
}

impl<'a> BitCompactFrameIter<'a> {
    pub fn new(frame: &'a BitCompactFrame) -> Self {
        Self {
            frame,
            buffer: StackVec::new(frame.first()),
            yielded: 0,
        }
    }
}

impl StreamingIterator for BitCompactFrameIter<'_> {
    type Item = [u8];

    fn advance(&mut self) {
        // Count rows, including identical rows whose packed size is zero bytes.
        if self.yielded < self.frame.length {
            self.frame.read_row(self.yielded, &mut self.buffer);
            self.yielded += 1;
        } else {
            self.yielded = self.frame.length + 1;
        }
    }

    fn get(&self) -> Option<&Self::Item> {
        (self.yielded > 0 && self.yielded <= self.frame.length).then(|| self.buffer.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_rows(compact: &BitCompactFrame, expected: &[&[u8]]) {
        let mut iter = compact.iter();
        assert!(iter.get().is_none());
        for (index, &row) in expected.iter().enumerate() {
            assert_eq!(iter.next(), Some(row));
            let mut decoded = vec![0xCC; row.len()];
            compact.read_row(index, &mut decoded);
            assert_eq!(decoded, row);
        }
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
    }

    #[test]
    fn packs_only_selected_bits_in_source_order() {
        let rows: [&[u8]; 3] = [&[0xAA, 0x55], &[0x2B, 0x57], &[0xAB, 0x55]];
        let mut frame = Frame::new(2, rows.len());
        for row in rows {
            frame.push(row);
        }
        let compact = BitCompactFrame::new_from(&frame);
        assert_eq!(compact.unit(), 2);
        assert_eq!(compact.len(), 3);
        assert_eq!(compact.compact_unit(), 1);
        // Mask, baseline, then three selected bits per subsequent row (LSB first).
        assert_eq!(compact.buffer(), &[0x81, 0x02, 0xAA, 0x55, 0b101, 0b011]);
        assert_rows(&compact, &rows);

        let mut restored = Frame::new(2, rows.len());
        compact.uncompact(&mut restored);
        assert_eq!(restored.length, frame.length);
        assert_eq!(restored.buffer, frame.buffer);
    }

    #[test]
    fn any_frame_edits_preserve_other_rows() {
        use super::super::{AnyFrame, Row, RowBuffer};

        // Cover both scratch paths and edits to the baseline or a packed row.
        for buffered in [false, true] {
            for index in [0, 1] {
                let mut expected = [[0xAA, 0x55], [0xAB, 0x55], [0xAA, 0x55]];
                let mut frame = Frame::new(2, expected.len());
                for row in &expected {
                    frame.push(row);
                }
                let mut compact = BitCompactFrame::new_from(&frame);
                // Varying-bit edits and no-ops reuse storage; constant-bit edits rebuild.
                for (byte, change, reuse) in
                    [(0, 1, true), (0, 0x80, false), (1, 2, false), (0, 0, true)]
                {
                    if !reuse {
                        // Rejection is atomic: even packed bytes and padding stay intact.
                        let before = compact.buffer().to_vec();
                        let mut edit = expected[index];
                        edit[byte] ^= change;
                        assert_eq!(compact.try_write_row(index, &edit), Err(NeedsRebuild));
                        assert_eq!(compact.buffer(), before);
                    }
                    let address = &*compact as *const BitCompactFrame;
                    let mask = compact.mask().to_vec();
                    let mut scratch = [0xCC; 2];
                    let mut row = Row {
                        init: false,
                        buffer: if buffered {
                            RowBuffer::Borrowed(&mut scratch)
                        } else {
                            RowBuffer::Owned(Box::default())
                        },
                        index,
                        frame: AnyFrame::BitCompact(&mut compact),
                    };
                    assert_eq!(row.get_mut(), &expected[index]);
                    row.get_mut()[byte] ^= change;
                    drop(row);
                    expected[index][byte] ^= change;
                    assert_rows(&compact, &[&expected[0], &expected[1], &expected[2]]);
                    if reuse {
                        assert!(std::ptr::eq(address, &*compact));
                        assert_eq!(&**compact.mask(), mask.as_slice());
                    } else {
                        assert_ne!(&**compact.mask(), mask.as_slice());
                    }
                }
            }
        }
    }

    #[test]
    fn rounds_each_row_up_to_whole_bytes() {
        // Cover both sides of byte boundaries, including a fully varying row.
        for varying_bits in 0usize..=24 {
            let mask = ((1u32 << varying_bits) - 1).to_le_bytes();
            let first = [0xA5, 0x5A, 0xC3];
            let second = std::array::from_fn::<_, 3, _>(|i| first[i] ^ mask[i]);
            let mut frame = Frame::new(3, 2);
            frame.push(&first);
            frame.push(&second);
            let mut compact = BitCompactFrame::new_from(&frame);
            assert_eq!(compact.compact_unit(), varying_bits.div_ceil(8));
            assert_eq!(compact.buffer().len(), 6 + varying_bits.div_ceil(8));
            if varying_bits % 8 != 0 {
                // Padding belongs to each row and must never contain source bits.
                assert_eq!(compact.buffer().last().unwrap() >> (varying_bits % 8), 0);
            }
            assert_rows(&compact, &[&first, &second]);
            let mut restored = Frame::new(3, 2);
            compact.uncompact(&mut restored);
            assert_eq!(restored.buffer, frame.buffer);
            let address = &*compact as *const BitCompactFrame;
            compact.try_write_row(1, &first).unwrap();
            assert!(std::ptr::eq(address, &*compact));
            assert_rows(&compact, &[&first, &first]);
            if varying_bits % 8 != 0 {
                assert_eq!(compact.buffer().last().unwrap() >> (varying_bits % 8), 0);
            }
        }
    }

    #[test]
    fn empty_single_and_identical_rows_terminate() {
        for length in [0, 1, 4] {
            let mut frame = Frame::new(3, length);
            for _ in 0..length {
                frame.push(&[7, 8, 9]);
            }
            let compact = BitCompactFrame::new_from(&frame);
            assert_eq!(compact.is_empty(), length == 0);
            assert_eq!(compact.compact_unit(), 0);
            assert_eq!(compact.buffer().len(), if length == 0 { 3 } else { 6 });
            assert_rows(&compact, &vec![&[7, 8, 9][..]; length]);
            let mut restored = Frame::new(3, length);
            compact.uncompact(&mut restored);
            assert_eq!(restored.length, length);
            assert_eq!(restored.buffer, frame.buffer);
        }
    }

    #[test]
    fn preserves_locked_bits_and_accounts_for_allocation_padding() {
        for unit in 1..=33 {
            let mut frame = Frame::new(unit, 1);
            let row = vec![0xAB; unit];
            frame.push(&row);
            frame.lock_editable(0, 1);
            let compact = BitCompactFrame::new_from(&frame);
            assert_eq!(compact.mask()[0], 0xFF);
            assert_eq!(compact.compact_unit(), 1);
            assert_rows(&compact, &[&row]);
            let layout = Layout::for_value(&*compact);
            assert_eq!(layout, allocation_layout(compact.buffer().len()));
            let stats = compact.compression_ratio();
            assert_eq!(stats.data, unit);
            assert_eq!(stats.compressed, unit);
            assert_eq!(stats.overalloc, 0);
            assert_eq!(stats.compressed + stats.overhead, layout.size());
        }
    }
}
