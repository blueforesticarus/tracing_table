use std::{any::TypeId, mem::ManuallyDrop};

use string_cache::Atom;

use crate::subscriber::{ValueData, ValueKind};

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

    pub fn write_any_value(&mut self, data: &ValueData, aligned: bool) {
        match data {
            ValueData::F64(v) => self.write_any(v, aligned),
            ValueData::I64(v) => self.write_any(v, aligned),
            ValueData::U64(v) => self.write_any(v, aligned),
            ValueData::I128(v) => self.write_any(v, aligned),
            ValueData::U128(v) => self.write_any(v, aligned),
            ValueData::Bool(v) => self.write_any(v, aligned),
            ValueData::Str(atom) => self.write_any(atom, aligned),
            ValueData::PrefixStr(atom, atom1) => {
                self.write_any(atom, aligned);
                self.write_any(atom1, aligned);
            }
        }
    }

    pub fn write_any_owned<T>(&mut self, data: T, aligned: bool) {
        self.write_any(&data, aligned);
        let _ = ManuallyDrop::new(data);
    }

    pub fn write_any<T>(&mut self, data: &T, aligned: bool) {
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
            // use scratchpad
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

    pub unsafe fn read_value(&mut self, kind: ValueKind, aligned: bool) -> ValueData {
        unsafe {
            match kind {
                ValueKind::F64 => ValueData::F64(*self.read_any(aligned)),
                ValueKind::I64 => ValueData::I64(*self.read_any(aligned)),
                ValueKind::U64 => ValueData::U64(*self.read_any(aligned)),
                ValueKind::I128 => ValueData::I128(*self.read_any(aligned)),
                ValueKind::U128 => ValueData::U128(*self.read_any(aligned)),
                ValueKind::Bool => ValueData::Bool(*self.read_any(aligned)),
                ValueKind::Str => {
                    ValueData::Str(self.read_any::<crate::subscriber::Atom>(aligned).clone())
                }
                ValueKind::PrefixStr => ValueData::PrefixStr(
                    self.read_any::<crate::subscriber::Atom>(aligned).clone(),
                    self.read_any::<crate::subscriber::Atom>(aligned).clone(),
                ),
            }
        }
    }
}
