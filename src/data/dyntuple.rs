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
            // String offsets occupy four bytes regardless of the platform pointer width.
            ValueKind::BufferOffset => size_of::<u32>(),
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

fn any_bytes<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
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

    /// The caller supplies offsets into its own storage.
    pub fn write_any_value(&mut self, data: ValueData, aligned: bool) {
        match data {
            ValueData::F64(v) => self.write_any(v, aligned),
            ValueData::I64(v) => self.write_any(v, aligned),
            ValueData::U64(v) => self.write_any(v, aligned),
            ValueData::I128(v) => self.write_any(v, aligned),
            ValueData::U128(v) => self.write_any(v, aligned),
            ValueData::Bool(v) => self.write_any(v, aligned),
            ValueData::BufferOffset(offset) => self.write_any(offset, aligned),
        }
    }

    // moves value into the row
    pub fn write_any<T: 'static + Unpin>(&mut self, data: T, aligned: bool) {
        unsafe {
            self.write_any_by_ref(&data, aligned);
        }
        let _ = ManuallyDrop::new(data);
    }

    // this is unsafe because loading is meant to be safe.
    unsafe fn write_any_by_ref<T>(&mut self, data: &T, aligned: bool) {
        if aligned {
            self.align_fill(align_of::<T>());
        }

        let start = self.offset;
        self.offset += std::mem::size_of::<T>();
        self.buffer[start..self.offset].copy_from_slice(unsafe {
            // I have serious gripes with this language
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

        let mut ptr = self.buffer[start..self.offset].as_ptr() as *const ();
        if ptr as usize % align_of::<T>() != 0 {
            // Reserve aligned storage before decoding unaligned packed fields.
            self.scratchpad.resize(size.div_ceil(size_of::<u128>()), 0);
            let scratchpad = self.scratchpad.as_mut_slice();
            let scratchpad = &mut bytemuck::cast_slice_mut::<u128, u8>(scratchpad)[..size];
            scratchpad[..size].copy_from_slice(&self.buffer[start..self.offset]);
            ptr = scratchpad.as_ptr() as *const ();
        }

        unsafe { &mut *(ptr as *mut T) }
    }

    pub unsafe fn read_any_owned<T: Unpin>(&mut self, aligned: bool) -> T {
        let value: T = unsafe { std::ptr::read(self.read_any(aligned) as *const T) };
        value
    }

    /// String fields return offsets; this layer does not resolve referenced storage.
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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_offsets_round_trip_unaligned() {
        let mut bytes = vec![0; 1 + ValueKind::BufferOffset.unit()];
        let mut packer = BytePacker::new(&mut bytes);
        packer.write_any_value(ValueData::Bool(true), false);
        packer.write_any_value(ValueData::BufferOffset(70_000), false);
        assert_eq!(bytes.len(), 5);
        assert_eq!(u32::from_ne_bytes(bytes[1..].try_into().unwrap()), 70_000);
        // The bool forces the following offset to be unaligned in a packed row.
        let mut packer = BytePacker::new(&mut bytes);
        unsafe {
            packer.skip::<bool>(false);
            assert!(matches!(
                packer.read_value(ValueKind::BufferOffset, false),
                ValueData::BufferOffset(70_000)
            ));
        }
    }
}
