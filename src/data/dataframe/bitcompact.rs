use std::{alloc::Layout, mem::MaybeUninit, ptr, slice};

use streaming_iterator::StreamingIterator;

use super::{
    CONTINUOUS_BIT_PACKING, CompressionStats, Frame, Mask, NeedsRebuild, StackVec, copy_bytes,
};
use crate::data::util::smallbox::Initializer;

/// Stores the mask and first row once, then only varying bits for later rows.
/// CONTINUOUS_BIT_PACKING selects consecutive bits or byte-aligned rows at build
/// time. Bits are ordered by source byte, then least-significant source bit.
#[repr(C)]
pub struct BitCompactFrame {
    // Widen these bounded counters before doing row or buffer arithmetic.
    unit: u16,
    length: u16,
    // Bound the packed stride to u16 bits and widen before offset arithmetic.
    // Keeping it here avoids rescanning the mask on every random read.
    packed_row_bits: u16,
    // Private storage stays uninitialized until construction writes every byte;
    // initialized boundary bytes may then be shared by adjacent packed rows.
    buffer: [MaybeUninit<u8>],
}

impl BitCompactFrame {
    pub fn new_from(frame: &Frame) -> Box<Self> {
        Self::initializer(frame).into_box()
    }

    /// Append initialized opaque bytes without adding a stored trailer length.
    pub fn new_from_with_trailer(frame: &Frame, trailer: &[u8]) -> Box<Self> {
        Self::initializer_with_trailer(frame, trailer).into_box()
    }

    /// Prepare packing without allocating; SmallBox can supply the final archive storage.
    pub(crate) fn initializer(frame: &Frame) -> Initializer<Self, impl FnOnce(*mut Self) + '_> {
        Self::initializer_with_trailer(frame, &[])
    }

    pub(crate) fn initializer_with_trailer<'a>(
        frame: &'a Frame,
        trailer: &'a [u8],
    ) -> Initializer<Self, impl FnOnce(*mut Self) + 'a> {
        Self::initializer_with_packing_and_trailer::<CONTINUOUS_BIT_PACKING>(frame, trailer)
    }

    // Keep both layouts testable in one build; production specializes this using
    // the public constant, without a runtime policy branch or per-block mode tag.
    #[cfg(test)]
    fn initializer_with_packing<const CONTINUOUS: bool>(
        frame: &Frame,
    ) -> Initializer<Self, impl FnOnce(*mut Self) + '_> {
        Self::initializer_with_packing_and_trailer::<CONTINUOUS>(frame, &[])
    }

    fn initializer_with_packing_and_trailer<'a, const CONTINUOUS: bool>(
        frame: &'a Frame,
        trailer: &'a [u8],
    ) -> Initializer<Self, impl FnOnce(*mut Self) + 'a> {
        assert!(frame.unit > 0, "frame unit must be nonzero");
        // Validate active-frame metadata before allocating the narrower archive.
        let unit = u16::try_from(frame.unit).expect("bit compact frame unit exceeds u16");
        let length = u16::try_from(frame.length).expect("bit compact frame length exceeds u16");
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
        let packed_row_bits = if CONTINUOUS {
            varying_bits
        } else {
            varying_bits.div_ceil(8) * 8
        };
        // Reject oversized masks (including byte-alignment rounding) before
        // allocating an archive; never truncate the bit stride.
        let stored_row_bits =
            u16::try_from(packed_row_bits).expect("packed row stride exceeds u16");
        let header_size = frame
            .unit
            .checked_mul(if frame.length == 0 { 1 } else { 2 })
            .expect("bit compact frame header size overflow");
        let buffer_size = frame
            .length
            .saturating_sub(1)
            .checked_mul(packed_row_bits)
            .and_then(|bits| bits.div_ceil(8).checked_add(header_size))
            .expect("bit compact frame size overflow");
        let allocation_bytes = buffer_size
            .checked_add(trailer.len())
            .expect("frame trailer overflow");
        let layout = allocation_layout(allocation_bytes);

        // SAFETY: The layout matches the counters and MaybeUninit tail. Initialize
        // all counters before borrowing the tail, and all payload bytes before
        // returning to whichever owner provided the storage.
        unsafe {
            Initializer::new(layout, allocation_bytes, move |raw: *mut Self| {
                ptr::addr_of_mut!((*raw).unit).write(unit);
                ptr::addr_of_mut!((*raw).length).write(length);
                ptr::addr_of_mut!((*raw).packed_row_bits).write(stored_row_bits);
                let compact = &mut *raw;
                copy_bytes(&mut compact.buffer[buffer_size..], trailer);
                copy_bytes(&mut compact.buffer[..header_size], &source[..header_size]);
                // Initialize shared boundary bytes before preserving their bits.
                // Only the final block byte can retain unused high zero bits.
                let packed = &mut compact.buffer[header_size..buffer_size];
                packed.fill(MaybeUninit::new(0));
                // SAFETY: Every byte in this disjoint packed tail was initialized.
                let packed = slice::from_raw_parts_mut(packed.as_mut_ptr().cast(), packed.len());
                for (index, row) in source[header_size..].chunks_exact(frame.unit).enumerate() {
                    pack_row(row, mask, packed, index * packed_row_bits);
                }
            })
        }
    }

    pub fn unit(&self) -> usize {
        usize::from(self.unit)
    }
    pub fn len(&self) -> usize {
        usize::from(self.length)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn compact_unit(&self) -> usize {
        // Rounded per-row width is informational, not a byte offset into the tail.
        self.packed_row_bits().div_ceil(8)
    }

    /// Distance between consecutive packed rows in bits (the first row is raw).
    pub fn packed_row_bits(&self) -> usize {
        usize::from(self.packed_row_bits)
    }

    /// Initialized row payload, excluding the opaque trailer and allocation padding.
    pub fn buffer(&self) -> &[u8] {
        // SAFETY: new_from writes the entire payload before returning it, and
        // private fields prevent callers from changing its initialization bounds.
        unsafe { slice::from_raw_parts(self.buffer.as_ptr().cast(), self.row_payload_len()) }
    }

    /// The DST slice length gives the exact allocation tail, excluding heap padding.
    /// Subtract the row payload implied by the header; no length field is needed.
    pub fn trailer(&self) -> &[u8] {
        let start = self.row_payload_len();
        // SAFETY: Every constructor initializes the entire DST tail, and the
        // checked payload length partitions that same allocation before padding.
        unsafe {
            slice::from_raw_parts(
                self.buffer.as_ptr().cast::<u8>().add(start),
                self.buffer.len() - start,
            )
        }
    }

    fn row_payload_len(&self) -> usize {
        self.unit() * if self.is_empty() { 1 } else { 2 }
            + (self.len().saturating_sub(1) * self.packed_row_bits()).div_ceil(8)
    }

    pub fn mask(&self) -> &Mask {
        Mask::get(&self.buffer()[..self.unit()])
    }

    pub fn first(&self) -> &[u8] {
        let end = if self.is_empty() {
            self.unit()
        } else {
            2 * self.unit()
        };
        &self.buffer()[self.unit()..end]
    }

    /// Decode one row into an existing buffer, without allocating or clearing it.
    pub fn read_row(&self, index: usize, output: &mut [u8]) {
        assert!(index < self.len(), "row index out of bounds");
        assert_eq!(output.len(), self.unit(), "row size mismatch");
        output.copy_from_slice(self.first());
        self.unpack_varying(index, output);
    }

    /// Decode just one column; packed positions are derived from the shared mask.
    pub fn read_column(&self, index: usize, offset: usize, output: &mut [u8]) {
        assert!(index < self.len() && offset + output.len() <= self.unit());
        output.copy_from_slice(&self.first()[offset..offset + output.len()]);
        if index == 0 {
            return;
        }
        let mask = self.mask();
        let mut position = (index - 1) * self.packed_row_bits()
            + mask[..offset]
                .iter()
                .map(|byte| byte.count_ones() as usize)
                .sum::<usize>();
        let packed = &self.buffer()[2 * self.unit()..];
        for (value, &varying) in output.iter_mut().zip(&mask[offset..]) {
            *value &= !varying;
            let mut bits = varying;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                *value |= ((packed[position / 8] >> (position % 8)) & 1) << bit;
                bits &= bits - 1;
                position += 1;
            }
        }
    }

    /// Update in place, or return NeedsRebuild without changes if a constant bit changes.
    pub fn try_write_row(&mut self, index: usize, row: &[u8]) -> Result<(), NeedsRebuild> {
        assert!(index < self.len(), "row index out of bounds");
        assert_eq!(row.len(), self.unit(), "row size mismatch");
        let first = self.first();
        let constants_changed = self
            .mask()
            .iter()
            .enumerate()
            .any(|(i, &mask)| (row[i] ^ first[i]) & !mask != 0);
        if constants_changed {
            return Err(NeedsRebuild);
        }

        let unit = self.unit();
        let packed_row_bits = self.packed_row_bits();
        let payload_len = self.row_payload_len();
        let (header, packed) = self.buffer[..payload_len].split_at_mut(2 * unit);
        if index == 0 {
            copy_bytes(&mut header[unit..], row);
        } else {
            // SAFETY: The mask is initialized and disjoint from the packed output.
            let mask = unsafe { slice::from_raw_parts(header.as_ptr().cast(), unit) };
            // SAFETY: Constructors initialize the complete packed tail. The mask
            // is disjoint and pack_row preserves neighboring rows' boundary bits.
            let packed =
                unsafe { slice::from_raw_parts_mut(packed.as_mut_ptr().cast(), packed.len()) };
            pack_row(row, mask, packed, (index - 1) * packed_row_bits);
        }
        Ok(())
    }

    fn unpack_varying(&self, index: usize, output: &mut [u8]) {
        if index == 0 {
            return;
        }
        let packed = &self.buffer()[2 * self.unit()..];
        let mut position = (index - 1) * self.packed_row_bits();
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
        assert_eq!(new.unit, self.unit(), "uncompact unit mismatch");
        assert!(
            new.capacity() >= self.len(),
            "uncompact destination is too small"
        );
        new.length = 0;
        new.buffer[..self.unit()].copy_from_slice(self.mask());
        for index in 0..self.len() {
            let start = (index + 1) * self.unit();
            // SAFETY: Seed the entire destination row before creating a readable
            // slice over it. The source and destination are disjoint allocations.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.first().as_ptr(),
                    new.buffer.as_mut_ptr().add(start),
                    self.unit(),
                );
            }
            self.unpack_varying(index, &mut new.buffer[start..start + self.unit()]);
            // Publish after writing; the copied mask already describes these rows.
            new.length += 1;
        }
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            data: self.len() * self.unit(),
            compressed: self.buffer().len() - self.unit(),
            overhead: size_of_val(self) - self.buffer().len() + self.unit(),
            overalloc: 0,
        }
    }
}

// Use the same packing order for construction and in-place row updates.
fn pack_row(row: &[u8], mask: &[u8], output: &mut [u8], start_bit: usize) {
    let mut offset = start_bit / 8;
    let mut used = start_bit % 8;
    // Preserve the preceding row's low bits when the start byte is shared.
    let mut packed = if used == 0 {
        0
    } else {
        output[offset] & ((1 << used) - 1)
    };
    for (&value, &varying) in row.iter().zip(mask) {
        let mut bits = varying;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            packed |= ((value >> bit) & 1) << used;
            used += 1;
            if used == 8 {
                output[offset] = packed;
                offset += 1;
                packed = 0;
                used = 0;
            }
            bits &= bits - 1;
        }
    }
    if used != 0 {
        // Preserve the following row's high bits when the end byte is shared.
        output[offset] = packed | (output[offset] & !((1 << used) - 1));
    }
}

fn allocation_layout(buffer_size: usize) -> Layout {
    // Match the three u16 fields and their natural alignment when dropping a DST.
    let size = size_of::<[u16; 3]>()
        .checked_add(buffer_size)
        .expect("bit compact frame allocation size overflow");
    // Match the header's natural alignment so Box deallocates with the same layout.
    Layout::from_size_align(size, align_of::<u16>())
        .expect("bit compact frame allocation exceeds layout limits")
        .pad_to_align()
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
        if self.yielded < self.frame.len() {
            self.frame.read_row(self.yielded, &mut self.buffer);
            self.yielded += 1;
        } else {
            self.yielded = self.frame.len() + 1;
        }
    }

    fn get(&self) -> Option<&Self::Item> {
        (self.yielded > 0 && self.yielded <= self.frame.len()).then(|| self.buffer.as_ref())
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
        if CONTINUOUS_BIT_PACKING {
            assert_eq!(compact.buffer(), &[0x81, 0x02, 0xAA, 0x55, 0b011101]);
        } else {
            assert_eq!(compact.buffer(), &[0x81, 0x02, 0xAA, 0x55, 0b101, 0b011]);
        }
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
    fn single_packed_row_rounds_to_whole_bytes() {
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
            // Odd payload lengths still need final padding for the u16 header.
            let layout = Layout::for_value(&*compact);
            assert_eq!(layout.align(), align_of::<u16>());
            assert_eq!(layout, allocation_layout(compact.buffer().len()));
            if varying_bits % 8 != 0 {
                // Final block padding must never contain source bits.
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
    fn both_packing_modes_preserve_neighbors_across_every_bit_boundary() {
        fn check<const CONTINUOUS: bool>() {
            for varying_bits in 0u32..=32 {
                let mask = u32::MAX
                    .checked_shr(32 - varying_bits)
                    .unwrap_or(0)
                    .to_le_bytes();
                for length in [2, 3, 8, 10, 17] {
                    let first = [0xA5, 0x5A, 0x3C, 0xC3];
                    let mut expected = vec![first; length];
                    // Force the complete mask on row one, then mixed bit patterns
                    // in neighboring rows to catch clobbered shared bytes.
                    for (index, row) in expected.iter_mut().enumerate().skip(1) {
                        for byte in 0..4 {
                            let pattern = if index == 1 {
                                0xFF
                            } else {
                                (index as u8).wrapping_mul(37 + byte as u8)
                            };
                            row[byte] ^= mask[byte] & pattern;
                        }
                    }
                    let mut frame = Frame::new(4, length);
                    for row in &expected {
                        frame.push(row);
                    }
                    let mut compact =
                        BitCompactFrame::initializer_with_packing::<CONTINUOUS>(&frame).into_box();
                    let stride = if CONTINUOUS {
                        varying_bits as usize
                    } else {
                        (varying_bits as usize).div_ceil(8) * 8
                    };
                    assert_eq!(compact.packed_row_bits(), stride);
                    assert_eq!(
                        compact.buffer().len(),
                        8 + ((length - 1) * stride).div_ceil(8)
                    );
                    for index in (0..length).rev() {
                        for byte in 0..4 {
                            expected[index][byte] ^= mask[byte];
                        }
                        // Changing row zero must only change baseline varying bits;
                        // every other row, including shared bytes, remains exact.
                        compact.try_write_row(index, &expected[index]).unwrap();
                        let rows: Vec<_> = expected.iter().map(|r| r.as_slice()).collect();
                        assert_rows(&compact, &rows);
                    }
                    let before = compact.buffer().to_vec();
                    if varying_bits < 32 {
                        let mut rejected = expected[1];
                        let bit = varying_bits as usize;
                        rejected[bit / 8] ^= 1 << (bit % 8);
                        assert_eq!(compact.try_write_row(1, &rejected), Err(NeedsRebuild));
                        assert_eq!(compact.buffer(), before);
                    }
                    let mut restored = Frame::new(4, length);
                    compact.uncompact(&mut restored);
                    for (index, row) in expected.iter().enumerate() {
                        assert_eq!(&restored.buffer[(index + 1) * 4..(index + 2) * 4], row);
                    }
                }
            }
        }
        check::<true>();
        check::<false>();
    }

    #[test]
    fn bit_stride_accepts_u16_boundary_in_both_packing_modes() {
        fn check<const CONTINUOUS: bool>() {
            let first = vec![0; 8192];
            let mut second = vec![0xFF; 8192];
            // Continuous mode fits 65,535 bits; byte alignment fits 65,528.
            second[8191] = if CONTINUOUS { 0x7F } else { 0 };
            let mut frame = Frame::new(first.len(), 2);
            frame.push(&first);
            frame.push(&second);
            let compact =
                BitCompactFrame::initializer_with_packing::<CONTINUOUS>(&frame).into_box();
            assert_eq!(
                compact.packed_row_bits(),
                if CONTINUOUS { 65_535 } else { 65_528 }
            );
            assert_rows(&compact, &[&first, &second]);
        }
        check::<true>();
        check::<false>();
    }

    #[test]
    fn oversized_bit_stride_is_rejected_instead_of_truncated() {
        let mut frame = Frame::new(8192, 2);
        frame.push(&vec![0; 8192]);
        frame.push(&vec![0xFF; 8192]);
        assert!(
            std::panic::catch_unwind(
                || BitCompactFrame::initializer_with_packing::<true>(&frame).into_box()
            )
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(
                || BitCompactFrame::initializer_with_packing::<false>(&frame).into_box()
            )
            .is_err()
        );

        // A mask that fits u16 bits still overflows after byte-alignment rounding.
        let mut frame = Frame::new(8192, 2);
        frame.push(&vec![0; 8192]);
        let mut second = vec![0xFF; 8192];
        second[8191] = 0x7F;
        frame.push(&second);
        assert!(
            std::panic::catch_unwind(
                || BitCompactFrame::initializer_with_packing::<false>(&frame).into_box()
            )
            .is_err()
        );
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
