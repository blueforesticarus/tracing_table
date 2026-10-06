use std::{any::TypeId, mem::ManuallyDrop};

// Shared value representations live with the byte packer so storage does not depend on tracing.

#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueKind {
    F64,
    I64,
    U64,
    I128,
    U128,
    Bool,
    Str,
    PrefixStr,
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
            ValueData::Str(_) => ValueKind::Str,
            ValueData::PrefixStr(_, _) => ValueKind::PrefixStr,
        }
    }
}

impl ValueKind {
    // The subscriber uses this size to allocate packed rows.
    pub(crate) fn unit(&self) -> usize {
        match self {
            ValueKind::F64 => size_of::<f64>(),
            ValueKind::I64 => size_of::<i64>(),
            ValueKind::U64 => size_of::<u64>(),
            ValueKind::I128 => size_of::<i128>(),
            ValueKind::U128 => size_of::<u128>(),
            ValueKind::Bool => size_of::<bool>(),
            ValueKind::Str => size_of::<Atom>(),
            ValueKind::PrefixStr => size_of::<(Atom, Atom)>(),
        }
    }
}

pub type Atom = string_cache::Atom<string_cache::EmptyStaticAtomSet>;

#[derive(Debug, Clone)]
pub enum ValueData {
    F64(f64),
    I64(i64),
    U64(u64),
    I128(i128),
    U128(u128),
    Bool(bool),
    Str(Atom),
    PrefixStr(Atom, Atom),
}

/// Helper to split string into prefixes, while updating prefix
pub fn split_update_prefix(prefix: &mut Option<Atom>, value: &str) -> Atom {
    if let Some(p) = prefix {
        let mut new_prefix_index = None; // in bytes
        for (p_char, value_char) in p.char_indices().zip(value.chars()) {
            if p_char.1 != value_char {
                new_prefix_index = Some(p_char.0);
                break;
            }
        }

        if new_prefix_index == Some(0) {
            // no overlap, reset prefix to the value
            *p = Atom::from(value);
            return Atom::from(value);
        }

        let n = new_prefix_index.unwrap_or(p.len());
        let suffix = &value[n..];
        return Atom::from(suffix);
    } else {
        *prefix = None;
        return Atom::from(value);
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

    /// Packed string values acquire one owned reference per logical row. The
    /// storage owner must release those references before destroying its bytes.
    pub fn write_any_value(&mut self, data: &ValueData, aligned: bool) {
        match data {
            ValueData::F64(v) => self.write_any(v, aligned),
            ValueData::I64(v) => self.write_any(v, aligned),
            ValueData::U64(v) => self.write_any(v, aligned),
            ValueData::I128(v) => self.write_any(v, aligned),
            ValueData::U128(v) => self.write_any(v, aligned),
            ValueData::Bool(v) => self.write_any(v, aligned),
            ValueData::Str(atom) => self.write_any_owned(atom.clone(), aligned),
            ValueData::PrefixStr(atom, atom1) => {
                // Each logical row owns a reference, including compacted constant rows.
                self.write_any_owned(atom.clone(), aligned);
                self.write_any_owned(atom1.clone(), aligned);
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

    /// # Safety
    /// Bytes must contain a valid T. Unaligned values borrow scratch bytes;
    /// changing those does not write back to the row. Do not drop borrowed owners.
    pub unsafe fn read_any<T>(&mut self, aligned: bool) -> &mut T {
        if aligned {
            self.align(align_of::<T>());
        }

        let size = size_of::<T>();
        let start = self.offset;
        self.offset += size;

        let ptr = self.buffer[start..self.offset].as_mut_ptr();
        if ptr as usize % align_of::<T>() == 0 {
            return unsafe { &mut *ptr.cast::<T>() };
        }
        // This scratch buffer owns bytes, not the values represented by them.
        // The caller must not drop a borrowed atom or retain the reference.
        assert!(align_of::<T>() <= align_of::<u128>());
        self.scratchpad.resize(size.div_ceil(size_of::<u128>()), 0);
        let bytes = bytemuck::cast_slice_mut::<u128, u8>(&mut self.scratchpad);
        bytes[..size].copy_from_slice(&self.buffer[start..self.offset]);
        unsafe { &mut *bytes.as_mut_ptr().cast::<T>() }
    }

    /// Read a value by move. The caller must own the stored value and never read
    /// it again as owned (or use Copy types). Packed rows need unaligned reads.
    pub unsafe fn read_any_owned<T: Unpin>(&mut self, aligned: bool) -> T {
        if aligned {
            self.align(align_of::<T>());
        }
        let start = self.offset;
        self.offset += size_of::<T>();
        let bytes = &self.buffer[start..self.offset];
        unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
    }

    /// # Safety
    /// The next bytes must match `kind` and retain live atom owners. Returned
    /// atoms are cloned; this does not transfer ownership away from the row.
    pub unsafe fn read_value(&mut self, kind: ValueKind, aligned: bool) -> ValueData {
        unsafe {
            match kind {
                ValueKind::F64 => ValueData::F64(*self.read_any(aligned)),
                ValueKind::I64 => ValueData::I64(*self.read_any(aligned)),
                ValueKind::U64 => ValueData::U64(*self.read_any(aligned)),
                ValueKind::I128 => ValueData::I128(*self.read_any(aligned)),
                ValueKind::U128 => ValueData::U128(*self.read_any(aligned)),
                ValueKind::Bool => ValueData::Bool(*self.read_any(aligned)),
                ValueKind::Str => ValueData::Str(self.read_any::<Atom>(aligned).clone()),
                ValueKind::PrefixStr => ValueData::PrefixStr(
                    self.read_any::<Atom>(aligned).clone(),
                    self.read_any::<Atom>(aligned).clone(),
                ),
            }
        }
    }
}
