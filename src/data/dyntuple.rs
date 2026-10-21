use std::{any::TypeId, mem::ManuallyDrop};

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueKind {
    F64,
    I64,
    U64,
    I128,
    U128,
    Bool,
    BufferOffset,
    /// Insertion index into the row's string buffer.
    BufferIndex,
}

impl From<ValueData> for ValueKind {
    fn from(value: ValueData) -> Self {
        From::from(&value)
    }
}

impl From<&ValueData> for ValueKind {
    fn from(value: &ValueData) -> Self {
        match value {
            ValueData::F64(_) => ValueKind::F64,
            ValueData::I64(_) => ValueKind::I64,
            ValueData::U64(_) => ValueKind::U64,
            ValueData::I128(_) => ValueKind::I128,
            ValueData::U128(_) => ValueKind::U128,
            ValueData::Bool(_) => ValueKind::Bool,
            ValueData::BufferOffset(_) => ValueKind::BufferOffset,
            ValueData::BufferIndex(_) => ValueKind::BufferIndex,
        }
    }
}

impl ValueKind {
    // Exposed so the example can derive packed field ranges.
    pub fn unit(&self) -> usize {
        match self {
            ValueKind::F64 => size_of::<f64>(),
            ValueKind::I64 => size_of::<i64>(),
            ValueKind::U64 => size_of::<u64>(),
            ValueKind::I128 => size_of::<i128>(),
            ValueKind::U128 => size_of::<u128>(),
            ValueKind::Bool => size_of::<bool>(),
            // Both string references occupy four bytes regardless of pointer width.
            ValueKind::BufferOffset | ValueKind::BufferIndex => size_of::<u32>(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ValueData {
    F64(f64),
    I64(i64),
    U64(u64),
    I128(i128),
    U128(u128),
    Bool(bool),
    /// A resolved byte offset. The caller supplies the buffer and resolves reads.
    BufferOffset(u32),
    /// Insertion index; resolves through ExpandedBuffer::get_index.
    BufferIndex(u32),
}

/// A value that can write its representation through the safe packing API.
/// Implementations can encode fields separately when their Rust layout has padding.
pub trait WriteValue: Sized {
    fn write_to(self, packer: &mut BytePacker<'_>, aligned: bool);
}

impl<T: bytemuck::NoUninit + Unpin> WriteValue for T {
    fn write_to(self, packer: &mut BytePacker<'_>, aligned: bool) {
        // SAFETY: NoUninit guarantees every representation byte is initialized and
        // readable as u8. Its Copy bound means there is no destructor to suppress.
        unsafe { packer.write_any(self, aligned) };
    }
}

impl WriteValue for ValueData {
    fn write_to(self, packer: &mut BytePacker<'_>, aligned: bool) {
        // Encode only the payload: the enum itself can contain uninitialized padding.
        match self {
            ValueData::F64(v) => packer.write_any_value(v, aligned),
            ValueData::I64(v) => packer.write_any_value(v, aligned),
            ValueData::U64(v) => packer.write_any_value(v, aligned),
            ValueData::I128(v) => packer.write_any_value(v, aligned),
            ValueData::U128(v) => packer.write_any_value(v, aligned),
            ValueData::Bool(v) => packer.write_any_value(v, aligned),
            ValueData::BufferOffset(offset) => packer.write_any_value(offset, aligned),
            ValueData::BufferIndex(index) => packer.write_any_value(index, aligned),
        }
    }
}

/// Write data into a byte buffer and take it out again
/// Supports Pod safely
/// Supports any Unpin type unsafely

pub struct Columns {
    columns: Vec<Column>,
}

#[derive(Debug, Clone, Copy)]
pub struct Column {
    type_id: TypeId,
    size: usize,
    align: usize,
    offset: Option<usize>,
}

pub struct ColumnMetadata {
    min: usize,
    max: usize,
    sorted: bool,
}

pub struct RefMut<'a> {
    column: Column,
    data: &'a mut [u8],
}

impl Column {
    fn for_type<T: 'static>() -> Self {
        return Self {
            type_id: TypeId::of::<T>(),
            size: std::mem::size_of::<T>(),
            align: align_of::<T>(),
            offset: None,
        };
    }

    fn for_val<T: 'static>(v: &T) -> Self {
        return Self {
            type_id: TypeId::of::<T>(),
            size: std::mem::size_of_val(v),
            align: align_of_val(v),
            offset: None,
        };
    }
}

/// a tool for sticking data into a byte buffer.
/// writes and reads must be called in exactly the same order
/// read is unsafe unless types are pod.
/// handles alignment issues with scratchpad
pub struct BytePacker<'a> {
    //offset is in bytes, not words
    offset: usize,

    //but the buffer is words
    buffer: &'a mut [u8],

    // for alignment issues
    scratchpad: Vec<u128>,
}

impl<'a> BytePacker<'a> {
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            offset: 0,
            buffer,
            scratchpad: Vec::with_capacity(0),
        }
    }

    // panics if out of space
    // who needs pod. just be carefull
    // pub fn write<T: Pod>(&mut self, data: &T) {
    //     let size = std::mem::align_of::<T>();
    //     self.offset = (self.offset + size - 1) & !(size - 1);
    //     assert_eq!(self.offset % size, 0);

    //     let offset = self.offset + std::mem::size_of::<T>();
    //     self.buffer[self.offset..offset].copy_from_slice(bytemuck::bytes_of(data));
    //     self.offset += offset;
    // }
    pub fn align(&mut self, align: usize) {
        self.offset = (self.offset + align - 1) & !(align - 1);
        assert_eq!(self.offset % align, 0);
    }
    pub fn align_fill(&mut self, align: usize) {
        let old = self.offset;
        self.align(align);
        self.buffer[old..self.offset].fill(0);
    }

    pub fn skip<T>(&mut self, aligned: bool) {
        if aligned {
            self.align(align_of::<T>());
        }

        let size = size_of::<T>();
        self.offset += size;
    }

    /// Safely pack a ValueData payload or a value with a verified byte representation.
    /// Buffer offsets are supplied by the caller; this layer does not resolve them.
    pub fn write_any_value(&mut self, data: impl WriteValue, aligned: bool) {
        data.write_to(self, aligned);
    }

    /// Move a value's raw representation into the buffer without dropping it.
    ///
    /// # Safety
    /// Every byte of data, including padding, must be initialized and valid to
    /// observe as u8. The caller must preserve ownership, lifetimes, aliasing, and
    /// pointer provenance when subsequently using the stored representation.
    /// This byte buffer does not run the stored value's destructor.
    ///
    /// ```compile_fail,E0133
    /// use tracing_table::data::dyntuple::BytePacker;
    /// let mut bytes = [0u8; 8];
    /// // Raw writes require an explicit unsafe call with a safety justification.
    /// BytePacker::new(&mut bytes).write_any(42u64, false);
    /// ```
    pub unsafe fn write_any<T: 'static + Unpin>(&mut self, data: T, aligned: bool) {
        // SAFETY: The caller guarantees an initialized, byte-readable representation.
        unsafe {
            self.write_any_by_ref(&data, aligned);
        }
        let _ = ManuallyDrop::new(data);
    }

    // The caller must guarantee the referenced representation is byte-readable.
    unsafe fn write_any_by_ref<T>(&mut self, data: &T, aligned: bool) {
        if aligned {
            self.align_fill(align_of::<T>());
        }

        let start = self.offset;
        self.offset += std::mem::size_of::<T>();
        self.buffer[start..self.offset].copy_from_slice(unsafe {
            // SAFETY: data is valid for size_of::<T>() bytes, and the caller
            // guarantees those bytes, including padding, are initialized.
            std::slice::from_raw_parts(data as *const T as *const u8, std::mem::size_of::<T>())
        });
    }

    pub unsafe fn read_any<T>(&mut self, aligned: bool) -> &mut T {
        if aligned {
            self.align(align_of::<T>());
        }

        let size = size_of::<T>();
        let start = self.offset;
        self.offset += size;

        let mut ptr = self.buffer[start..self.offset].as_mut_ptr() as *mut ();
        if ptr as usize % align_of::<T>() != 0 {
            // Vec<u128> only guarantees u128 alignment. Reject stricter requirements
            // before constructing a typed reference to scratch storage.
            assert!(
                align_of::<T>() <= align_of::<u128>(),
                "BytePacker scratchpad cannot satisfy the requested alignment"
            );
            // Reserve aligned storage before decoding unaligned packed fields.
            self.scratchpad.resize(size.div_ceil(size_of::<u128>()), 0);
            let scratchpad = self.scratchpad.as_mut_slice();
            let scratchpad = &mut bytemuck::cast_slice_mut::<u128, u8>(scratchpad)[..size];
            scratchpad[..size].copy_from_slice(&self.buffer[start..self.offset]);
            ptr = scratchpad.as_mut_ptr() as *mut ();
        }

        // SAFETY: Both pointers come from mutable slices, preserving write permissions.
        // The returned reference borrows this packer exclusively, keeping its storage
        // alive and unavailable for reuse. Alignment is checked for the row pointer;
        // scratch alignment follows from u128 storage and the assertion above.
        // The caller must ensure T's validity and ownership/aliasing requirements.
        unsafe { &mut *(ptr as *mut T) }
    }

    pub unsafe fn read_any_owned<T: Unpin>(&mut self, aligned: bool) -> T {
        let value: T = unsafe { std::ptr::read(self.read_any(aligned) as *const T) };
        value
    }

    /// String fields return unresolved offsets or indices; this layer does not resolve referenced storage.
    pub unsafe fn read_value(&mut self, kind: ValueKind, aligned: bool) -> ValueData {
        unsafe {
            match kind {
                ValueKind::F64 => ValueData::F64(*self.read_any(aligned)),
                ValueKind::I64 => ValueData::I64(*self.read_any(aligned)),
                ValueKind::U64 => ValueData::U64(*self.read_any(aligned)),
                ValueKind::I128 => ValueData::I128(*self.read_any(aligned)),
                ValueKind::U128 => ValueData::U128(*self.read_any(aligned)),
                ValueKind::Bool => ValueData::Bool(*self.read_any(aligned)),
                ValueKind::BufferOffset => ValueData::BufferOffset(*self.read_any(aligned)),
                ValueKind::BufferIndex => ValueData::BufferIndex(*self.read_any(aligned)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These values require stricter alignment than the u128 scratchpad guarantees.
    #[repr(C, align(32))]
    struct Overaligned([u8; 32]);

    #[test]
    fn mutable_read_supports_overaligned_values_in_place() {
        let mut value = Overaligned([7; 32]);
        let mut packer = BytePacker::new(&mut value.0);
        // SAFETY: The bytes belong to an initialized Overaligned value, and the
        // packer exclusively borrows its storage at the original aligned address.
        unsafe { packer.read_any::<Overaligned>(false).0[0] = 42 };
        assert_eq!(value.0[0], 42);
    }

    #[test]
    #[should_panic(expected = "scratchpad cannot satisfy the requested alignment")]
    fn mutable_read_rejects_overaligned_scratch_storage() {
        // The first byte is aligned to 32; skipping it forces the scratchpad path.
        #[repr(align(32))]
        struct AlignedSource([u8; 33]);
        let mut storage = AlignedSource([0; 33]);
        let mut packer = BytePacker::new(&mut storage.0[1..]);
        // SAFETY: All bytes are initialized and valid for Overaligned. The guard
        // must reject the scratchpad's insufficient alignment before making a reference.
        unsafe { packer.read_any::<Overaligned>(false) };
    }

    #[test]
    fn mutable_read_updates_aligned_buffer() {
        // A u64 array guarantees the field's alignment without a custom allocator.
        let mut words = [7u64];
        let mut packer = BytePacker::new(bytemuck::cast_slice_mut(&mut words));
        // SAFETY: The bytes hold an initialized, aligned u64 with exclusive access.
        unsafe { *packer.read_any::<u64>(false) = 42 };
        assert_eq!(words[0], 42);
    }

    #[test]
    fn mutable_read_supports_unaligned_scratch_storage() {
        // Offset from aligned storage to force the scratchpad path deterministically.
        let mut words = [0u64; 2];
        let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut words);
        let field = &mut bytes[1..1 + size_of::<u64>()];
        field.copy_from_slice(&7u64.to_ne_bytes());
        let mut packer = BytePacker::new(field);
        // SAFETY: The bytes hold a valid u64; u128 scratch storage supplies its
        // required alignment, and the packer has exclusive access to the bytes.
        let value = unsafe { packer.read_any::<u64>(false) };
        assert_eq!(*value, 7);
        *value = 42;
        assert_eq!(*value, 42);
    }

    #[test]
    fn buffer_references_round_trip_unaligned() {
        for value in [
            ValueData::BufferOffset(70_000),
            ValueData::BufferIndex(70_000),
        ] {
            let kind = ValueKind::from(value);
            let mut bytes = vec![0; 1 + kind.unit()];
            let mut packer = BytePacker::new(&mut bytes);
            packer.write_any_value(ValueData::Bool(true), false);
            packer.write_any_value(value, false);
            assert_eq!(bytes.len(), 5);
            assert_eq!(u32::from_ne_bytes(bytes[1..].try_into().unwrap()), 70_000);
            // A leading bool forces the reference to be unaligned in a packed row.
            let mut packer = BytePacker::new(&mut bytes);
            unsafe {
                packer.skip::<bool>(false);
                let decoded = packer.read_value(kind, false);
                assert_eq!(
                    std::mem::discriminant(&decoded),
                    std::mem::discriminant(&value)
                );
                assert!(matches!(
                    decoded,
                    ValueData::BufferOffset(70_000) | ValueData::BufferIndex(70_000)
                ));
            }
        }
    }
}
