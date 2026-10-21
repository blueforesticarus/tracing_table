//! Read tracing's retained static objects without executing the inspected ELF.
//! Rust layouts come from the object's DWARF, never from the scanner's ABI.

use super::{CallsiteKind, Result, dwarf::dwarf_name};
use gimli::{AttributeValue as Attr, Reader};
use object::{Object, ObjectSection, ObjectSymbol, ObjectSymbolTable};
use std::{collections::BTreeMap, rc::Rc};

/// File-backed virtual memory, with dynamic pointer relocations applied at load bias zero.
pub(super) struct Memory<'a> {
    regions: Vec<(u64, &'a [u8])>,
    pointers: BTreeMap<u64, Option<u64>>,
    little: bool,
    pointer_size: u64,
}

impl<'a> Memory<'a> {
    pub(super) fn new(elf: &object::File<'a>) -> Result<Self> {
        let mut regions = Vec::new();
        for section in elf.sections() {
            if matches!(section.flags(), object::SectionFlags::Elf { sh_flags }
                if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0)
                && section.kind() != object::SectionKind::UninitializedData
            {
                regions.push((section.address(), section.data()?));
            }
        }
        regions.sort_by_key(|(address, _)| *address);
        let mut memory = Self {
            regions,
            pointers: BTreeMap::new(),
            little: elf.is_little_endian(),
            pointer_size: if elf.is_64() { 8 } else { 4 },
        };
        if let Some(relocations) = elf.dynamic_relocations() {
            for (address, relocation) in relocations {
                // Object's generic Relative kind means S+A-P, not ELF's B+A.
                let relative = match (elf.architecture(), relocation.flags()) {
                    (object::Architecture::X86_64, object::RelocationFlags::Elf { r_type }) => {
                        r_type == object::elf::R_X86_64_RELATIVE
                    }
                    (object::Architecture::I386, object::RelocationFlags::Elf { r_type }) => {
                        r_type == object::elf::R_386_RELATIVE
                    }
                    (object::Architecture::Aarch64, object::RelocationFlags::Elf { r_type }) => {
                        r_type == object::elf::R_AARCH64_RELATIVE
                    }
                    (object::Architecture::Arm, object::RelocationFlags::Elf { r_type }) => {
                        r_type == object::elf::R_ARM_RELATIVE
                    }
                    (
                        object::Architecture::Riscv32 | object::Architecture::Riscv64,
                        object::RelocationFlags::Elf { r_type },
                    ) => r_type == object::elf::R_RISCV_RELATIVE,
                    _ => false,
                };
                let addend = if relocation.has_implicit_addend() {
                    memory.integer(address, memory.pointer_size).ok()
                } else {
                    Some(relocation.addend() as u64)
                };
                let value = if relative {
                    addend
                } else if relocation.kind() == object::RelocationKind::Absolute {
                    match relocation.target() {
                        object::RelocationTarget::Symbol(index) => elf
                            .dynamic_symbol_table()
                            .and_then(|symbols| symbols.symbol_by_index(index).ok())
                            .filter(|symbol| symbol.is_definition())
                            .and_then(|symbol| addend.map(|a| symbol.address().wrapping_add(a))),
                        object::RelocationTarget::Absolute => addend,
                        _ => None,
                    }
                } else {
                    None
                };
                // Keep unsupported relocations: reading their unrelocated zero as
                // Option::None would silently manufacture incorrect metadata.
                memory.pointers.insert(address, value);
            }
        }
        // Packed RELR relocations already store B-relative addends in the image;
        // reading those words at load bias zero needs no modification.
        Ok(memory)
    }

    fn bytes(&self, address: u64, length: u64) -> Result<&[u8]> {
        let index = self.regions.partition_point(|(start, _)| *start <= address);
        let (start, data) = index
            .checked_sub(1)
            .and_then(|i| self.regions.get(i))
            .ok_or_else(|| format!("metadata address {address:#x} is not file-backed"))?;
        let offset = usize::try_from(address - start)?;
        let end = offset
            .checked_add(usize::try_from(length)?)
            .ok_or("metadata range overflow")?;
        data.get(offset..end).ok_or_else(|| {
            format!("metadata range at {address:#x} is outside its ELF section").into()
        })
    }

    fn integer(&self, address: u64, size: u64) -> Result<u64> {
        if !matches!(size, 1 | 2 | 4 | 8) {
            return Err(format!("unsupported metadata integer width {size}").into());
        }
        let bytes = self.bytes(address, size)?;
        let mut value = 0;
        for index in 0..bytes.len() {
            let index = if self.little {
                bytes.len() - index - 1
            } else {
                index
            };
            value = (value << 8) | u64::from(bytes[index]);
        }
        Ok(value)
    }

    fn scalar(&self, address: u64, size: u64) -> Result<u64> {
        if size == self.pointer_size
            && let Some(value) = self.pointers.get(&address)
        {
            self.bytes(address, size)?;
            return value.ok_or_else(|| {
                format!("unsupported or unresolved metadata relocation at {address:#x}").into()
            });
        }
        self.integer(address, size)
    }
}

type Id<R> = gimli::DebugInfoOffset<<R as Reader>::Offset>;

#[derive(Clone)]
struct Field<R: Reader> {
    name: String,
    offset: u64,
    ty: Id<R>,
}

struct Variant<R: Reader> {
    discriminant: Option<u64>,
    field: Field<R>,
}

struct Layout<R: Reader> {
    tag: gimli::DwTag,
    name: Option<String>,
    size: Option<u64>,
    target: Option<Id<R>>,
    fields: Vec<Field<R>>,
    discriminant: Option<Field<R>>,
    variants: Vec<Variant<R>>,
}

#[derive(Clone, Copy)]
struct Value<R: Reader> {
    address: u64,
    ty: Id<R>,
}

/// Cache just the layouts used in this compilation unit, not every DWARF type.
pub(super) struct Decoder<'a, R: Reader> {
    unit: gimli::UnitRef<'a, R>,
    layouts: BTreeMap<Id<R>, Rc<Layout<R>>>,
}

// Named results make the boundary between layout decoding and extraction clear.
pub(super) struct Metadata {
    pub file: String,
    pub line: u32,
    pub module_path: Option<String>,
    pub kind: CallsiteKind,
    pub has_message: bool,
}

impl<'a, R: Reader> Decoder<'a, R> {
    pub(super) fn new(unit: gimli::UnitRef<'a, R>) -> Self {
        Self {
            unit,
            layouts: BTreeMap::new(),
        }
    }

    fn reference(unit: gimli::UnitRef<'_, R>, attr: Attr<R>) -> Result<Id<R>> {
        match attr {
            Attr::UnitRef(offset) => offset
                .to_debug_info_offset(&unit.header)
                .ok_or_else(|| "metadata type is not in .debug_info".into()),
            Attr::DebugInfoRef(offset) => Ok(offset),
            _ => Err("unsupported DWARF metadata type reference".into()),
        }
    }

    fn field(
        unit: gimli::UnitRef<'_, R>,
        entry: &gimli::DebuggingInformationEntry<R>,
    ) -> Result<Field<R>> {
        let offset = entry
            .attr_value(gimli::DW_AT_data_member_location)
            .and_then(|value| value.udata_value())
            .ok_or("metadata member has no constant DWARF offset")?;
        Ok(Field {
            name: dwarf_name(unit, entry)?.unwrap_or_default(),
            offset,
            ty: Self::reference(
                unit,
                entry
                    .attr_value(gimli::DW_AT_type)
                    .ok_or("metadata member has no type")?,
            )?,
        })
    }

    fn read_layout(
        unit: gimli::UnitRef<'_, R>,
        offset: gimli::UnitOffset<R::Offset>,
    ) -> Result<Layout<R>> {
        let mut tree = unit.entries_tree(Some(offset))?;
        let root = tree.root()?;
        let entry = root.entry();
        let mut layout = Layout {
            tag: entry.tag(),
            name: dwarf_name(unit, entry)?,
            size: entry
                .attr_value(gimli::DW_AT_byte_size)
                .and_then(|v| v.udata_value())
                .or_else(|| {
                    (entry.tag() == gimli::DW_TAG_pointer_type)
                        .then_some(u64::from(unit.encoding().address_size))
                }),
            target: entry
                .attr_value(gimli::DW_AT_type)
                .map(|v| Self::reference(unit, v))
                .transpose()?,
            fields: Vec::new(),
            discriminant: None,
            variants: Vec::new(),
        };
        let mut children = root.children();
        while let Some(child) = children.next()? {
            if child.entry().tag() == gimli::DW_TAG_member {
                layout.fields.push(Self::field(unit, child.entry())?);
            } else if child.entry().tag() == gimli::DW_TAG_variant_part {
                let discr = child
                    .entry()
                    .attr_value(gimli::DW_AT_discr)
                    .ok_or("metadata option has no discriminant")?;
                let discr = Self::reference(unit, discr)?
                    .to_unit_offset(&unit.header)
                    .ok_or("metadata discriminant is outside its unit")?;
                layout.discriminant = Some(Self::field(unit, &unit.entry(discr)?)?);
                let mut variants = child.children();
                while let Some(variant) = variants.next()? {
                    if variant.entry().tag() != gimli::DW_TAG_variant {
                        continue;
                    }
                    let discriminant = variant
                        .entry()
                        .attr_value(gimli::DW_AT_discr_value)
                        .map(|v| {
                            v.udata_value()
                                .ok_or("unsupported metadata option discriminant")
                        })
                        .transpose()?;
                    let mut fields = variant.children();
                    while let Some(field) = fields.next()? {
                        if field.entry().tag() == gimli::DW_TAG_member {
                            layout.variants.push(Variant {
                                discriminant,
                                field: Self::field(unit, field.entry())?,
                            });
                        }
                    }
                }
            }
        }
        Ok(layout)
    }

    fn layout(&mut self, ty: Id<R>) -> Result<Rc<Layout<R>>> {
        if let Some(layout) = self.layouts.get(&ty) {
            return Ok(layout.clone());
        }
        let layout = if let Some(offset) = ty.to_unit_offset(&self.unit.header) {
            Self::read_layout(self.unit, offset)?
        } else {
            // DW_FORM_ref_addr may point across compilation units. Only load the
            // referenced unit, and cache its resulting owned layout as usual.
            let mut headers = self.unit.dwarf.units();
            let mut found = None;
            while let Some(header) = headers.next()? {
                if let Some(offset) = ty.to_unit_offset(&header) {
                    let unit = self.unit.dwarf.unit(header)?;
                    found = Some(Self::read_layout(unit.unit_ref(self.unit.dwarf), offset)?);
                    break;
                }
            }
            found.ok_or("metadata type reference is outside .debug_info")?
        };
        let layout = Rc::new(layout);
        self.layouts.insert(ty, layout.clone());
        Ok(layout)
    }

    fn normalized(&mut self, mut ty: Id<R>) -> Result<Id<R>> {
        // Bound malformed typedef/qualifier cycles instead of recursing forever.
        for _ in 0..32 {
            let layout = self.layout(ty)?;
            if !matches!(
                layout.tag,
                gimli::DW_TAG_typedef
                    | gimli::DW_TAG_const_type
                    | gimli::DW_TAG_volatile_type
                    | gimli::DW_TAG_restrict_type
            ) {
                return Ok(ty);
            }
            ty = layout
                .target
                .ok_or("DWARF metadata type alias has no target")?;
        }
        Err("cycle in DWARF metadata types".into())
    }

    fn at_field(&mut self, value: Value<R>, field: &Field<R>) -> Result<Value<R>> {
        let parent_ty = self.normalized(value.ty)?;
        let parent = self.layout(parent_ty)?;
        let ty = self.normalized(field.ty)?;
        let size = self
            .layout(ty)?
            .size
            .ok_or("metadata field has no DWARF size")?;
        let end = field
            .offset
            .checked_add(size)
            .ok_or("metadata member range overflow")?;
        if end > parent.size.ok_or("metadata struct has no DWARF size")? {
            return Err("metadata member lies outside its DWARF struct".into());
        }
        Ok(Value {
            address: value
                .address
                .checked_add(field.offset)
                .ok_or("metadata address overflow")?,
            ty,
        })
    }

    fn member(&mut self, value: Value<R>, name: &str) -> Result<Value<R>> {
        let ty = self.normalized(value.ty)?;
        let layout = self.layout(ty)?;
        let field = layout
            .fields
            .iter()
            .find(|field| field.name == name)
            .ok_or_else(|| format!("DWARF metadata type {:?} has no {name} member", layout.name))?;
        self.at_field(value, field)
    }

    fn number(&mut self, memory: &Memory<'_>, value: Value<R>) -> Result<u64> {
        let ty = self.normalized(value.ty)?;
        memory.scalar(
            value.address,
            self.layout(ty)?
                .size
                .ok_or("metadata integer has no DWARF size")?,
        )
    }

    fn option(&mut self, memory: &Memory<'_>, value: Value<R>) -> Result<Option<Value<R>>> {
        let ty = self.normalized(value.ty)?;
        let layout = self.layout(ty)?;
        let discr = self.at_field(
            value.clone(),
            layout
                .discriminant
                .as_ref()
                .ok_or("metadata option has no DWARF variants")?,
        )?;
        // Niche discriminants may be relocated pointers; scalar() resolves those.
        let discr = self.number(memory, discr)?;
        let selected = layout
            .variants
            .iter()
            .find(|v| v.discriminant == Some(discr))
            .or_else(|| layout.variants.iter().find(|v| v.discriminant.is_none()))
            .ok_or("invalid metadata option discriminant")?;
        match selected.field.name.as_str() {
            "None" => Ok(None),
            "Some" => {
                let payload = self.at_field(value, &selected.field)?;
                Ok(Some(self.member(payload, "__0")?))
            }
            _ => Err("unsupported metadata option variant".into()),
        }
    }

    fn string(&mut self, memory: &Memory<'_>, value: Value<R>) -> Result<String> {
        let data = self.member(value.clone(), "data_ptr")?;
        let length = self.member(value, "length")?;
        let address = self.number(memory, data)?;
        let length = self.number(memory, length)?;
        if length == 0 {
            return Ok(String::new());
        }
        Ok(std::str::from_utf8(memory.bytes(address, length)?)?.to_owned())
    }

    fn has_message(&mut self, memory: &Memory<'_>, metadata: Value<R>) -> Result<bool> {
        let fields = self.member(metadata, "fields")?;
        let names = self.member(fields, "names")?;
        let length = self.member(names.clone(), "length")?;
        let length = self.number(memory, length)?;
        if length == 0 {
            return Ok(false);
        }
        let data = self.member(names, "data_ptr")?;
        let address = self.number(memory, data.clone())?;
        let ty = self
            .layout(data.ty)?
            .target
            .ok_or("field names data is not a pointer")?;
        let size = self
            .layout(ty)?
            .size
            .ok_or("field name has no DWARF size")?;
        // Validate the entire slice before iterating, including corrupt lengths.
        if size == 0 {
            return Err("field name has zero size".into());
        }
        memory.bytes(
            address,
            length
                .checked_mul(size)
                .ok_or("field names size overflow")?,
        )?;
        for index in 0..length {
            if self.string(
                memory,
                Value {
                    address: address
                        .checked_add(index * size)
                        .ok_or("field name address overflow")?,
                    ty,
                },
            )? == "message"
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn is_callsite(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<R>,
    ) -> Result<bool> {
        let Some(attr) = entry.attr_value(gimli::DW_AT_type) else {
            return Ok(false);
        };
        let ty = Self::reference(self.unit, attr)?;
        let ty = self.normalized(ty)?;
        Ok(self.layout(ty)?.name.as_deref() == Some("DefaultCallsite"))
    }

    /// Resolve __CALLSITE -> meta -> canonical location, using only DWARF types.
    /// Missing locations describe discarded objects and are skipped; an unsupported
    /// retained object returns an error rather than silently using inferred data.
    pub(super) fn read_callsite(
        &mut self,
        memory: &Memory<'_>,
        entry: &gimli::DebuggingInformationEntry<R>,
    ) -> Result<Option<Metadata>> {
        let expression = match entry.attr_value(gimli::DW_AT_location) {
            None => return Ok(None),
            Some(Attr::Exprloc(expression)) => expression,
            Some(_) => return Err("unsupported tracing static DWARF location attribute".into()),
        };
        let mut operations = expression.operations(self.unit.encoding());
        let address = match operations.next()? {
            Some(gimli::Operation::Address { address }) => address,
            Some(gimli::Operation::AddressIndex { index }) => self.unit.address(index)?,
            _ => return Err("unsupported tracing static DWARF location expression".into()),
        };
        if operations.next()?.is_some() {
            return Err("unsupported tracing static DWARF location expression".into());
        }
        if address == 0 {
            return Ok(None);
        }
        let ty = Self::reference(
            self.unit,
            entry
                .attr_value(gimli::DW_AT_type)
                .ok_or("tracing callsite has no DWARF type")?,
        )?;
        let meta = self.member(Value { address, ty }, "meta")?;
        let address = self.number(memory, meta.clone())?;
        let ty = self
            .layout(meta.ty)?
            .target
            .ok_or("tracing meta member is not a pointer")?;
        let value = Value { address, ty };
        let kind = self.member(value.clone(), "kind")?;
        let bits = self.member(kind, "__0")?;
        let bits = self.number(memory, bits)?;
        // tracing-core Kind uses EVENT_BIT=1 and SPAN_BIT=2. Offsets and widths
        // still come from this executable's DWARF, not the scanner's native ABI.
        let kind = if bits & 2 != 0 {
            CallsiteKind::Span
        } else {
            CallsiteKind::Event
        };
        let has_message = bits & 1 != 0 && self.has_message(memory, value.clone())?;
        let file = self.member(value.clone(), "file")?;
        let file = self
            .option(memory, file)?
            .ok_or("tracing metadata has no file")?;
        let file = self.string(memory, file)?;
        let line = self.member(value.clone(), "line")?;
        let line = self
            .option(memory, line)?
            .ok_or("tracing metadata has no line")?;
        let line = u32::try_from(self.number(memory, line)?)?;
        let module = self.member(value, "module_path")?;
        let module = self
            .option(memory, module)?
            .map(|v| self.string(memory, v))
            .transpose()?;
        Ok(Some(Metadata {
            file,
            line,
            module_path: module,
            kind,
            has_message,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_respects_target_width_endianness_and_bounds() -> Result<()> {
        // Simulate cross-target decoding; the scanner must not use native loads.
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8];
        for (little, expected) in [(true, 0x0807060504030201), (false, 0x0102030405060708)] {
            let mut memory = Memory {
                regions: vec![(0x1000, &bytes)],
                pointers: BTreeMap::new(),
                little,
                pointer_size: 8,
            };
            assert_eq!(memory.scalar(0x1000, 8)?, expected);
            memory.pointer_size = 4;
            assert_eq!(
                memory.scalar(0x1000, 4)?,
                if little { 0x04030201 } else { 0x01020304 }
            );
            assert!(memory.bytes(0x0fff, 1).is_err());
            assert!(memory.bytes(0x1007, 2).is_err());
            assert!(memory.bytes(0x1000, u64::MAX).is_err());
            assert!(memory.scalar(0x1000, 3).is_err());
        }
        Ok(())
    }

    #[test]
    fn relocated_niches_do_not_mistake_unrelocated_zero_for_none() -> Result<()> {
        // PIE filename pointers may be all zero in the file. A resolved pointer
        // must select Some; an unsupported relocation must fail, never select None.
        let bytes = [0; 8];
        let mut memory = Memory {
            regions: vec![(0x1000, &bytes)],
            pointers: BTreeMap::new(),
            little: true,
            pointer_size: 8,
        };
        memory.pointers.insert(0x1000, Some(0x2000));
        assert_eq!(memory.scalar(0x1000, 8)?, 0x2000);
        memory.pointers.insert(0x1000, None);
        assert!(memory.scalar(0x1000, 8).is_err());
        Ok(())
    }
}
