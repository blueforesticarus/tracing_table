//! DWARF discovery, framework matchers, and extraction scope evidence.
//! Source parsing and artifact formats are independent of this stage.

use super::{
    Callsite, CallsiteKind, LookupError, NO_MESSAGE, Result, SOURCE_NOT_EXAMINED, SPAN_TEMPLATE,
    SourceLocation, static_metadata,
};
use gimli::AttributeValue as Attr;
use object::{Object, ObjectSection};
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

// ---- Shared DWARF loading and locations ------------------------------------

// Preserve framework-specific canonical filenames; modules distinguish reused files.
type Sites = BTreeMap<(String, u32, bool, Option<String>), Callsite>;

/// Read tracing's static metadata and find log candidates, without opening source.
/// Tracing uses DWARF layouts/ELF data; log retains its namespace/location matcher.
/// `lookup_templates` confirms log candidates against source macro invocations.
pub fn extract(elf: &object::File<'_>) -> Result<Vec<Callsite>> {
    if elf.format() != object::BinaryFormat::Elf || elf.kind() == object::ObjectKind::Relocatable {
        return Err("expected a linked ELF executable or shared library".into());
    }
    // Keep decompressed sections alive while gimli borrows endian-aware readers.
    let sections = gimli::DwarfSections::load(|id: gimli::SectionId| {
        elf.section_by_name(id.name())
            .map_or(Ok(std::borrow::Cow::Borrowed(&[][..])), |s| {
                s.uncompressed_data()
            })
    })?;
    let endian = if elf.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };
    let dwarf = sections.borrow(|s| gimli::EndianSlice::new(s, endian));
    if elf
        .section_by_name(".debug_info")
        .or_else(|| elf.section_by_name(".zdebug_info"))
        .is_none()
    {
        return Err(
            "ELF has no DWARF .debug_info; rebuild with debug = 2 and without stripping".into(),
        );
    }
    let memory = static_metadata::Memory::new(elf)?;
    let mut units = dwarf.units();
    let mut headers = Vec::new();
    while let Some(header) = units.next()? {
        headers.push(header);
    }

    // Workers borrow immutable ELF/DWARF data and own all mutable decoder state.
    // Collecting this indexed iterator keeps unit order, including the first
    // error, regardless of which worker finishes first. Only owned data crosses
    // threads; retain the existing public error type by transporting messages.
    let results: Vec<Result<Sites, String>> = headers
        .into_par_iter()
        .map(|header| {
            let unit = dwarf.unit(header).map_err(|error| error.to_string())?;
            extract_unit(unit.unit_ref(&dwarf), &memory).map_err(|error| error.to_string())
        })
        .collect();
    let mut sites = Sites::new();
    for result in results {
        merge_sites(&mut sites, result?);
    }
    Ok(sites.into_values().collect())
}

// Keep framework discovery separate from scheduling so one worker is also a
// useful reference mode (`--threads 1`) for debugging and output comparisons.
fn extract_unit<R: gimli::Reader>(
    unit: gimli::UnitRef<'_, R>,
    memory: &static_metadata::Memory<'_>,
) -> Result<Sites> {
    let candidates = scan_candidates(unit)?;
    if !candidates.tracing && candidates.level_types.is_empty() {
        return Ok(Sites::new());
    }
    let mut sites = Sites::new();
    let scopes = dwarf_scopes(unit)?;
    if candidates.tracing {
        extract_tracing(unit, &scopes, memory, &mut sites)?;
    }
    if !candidates.level_types.is_empty() {
        extract_log(unit, &scopes, &candidates.level_types, &mut sites)?;
    }
    Ok(sites)
}

fn merge_sites(sites: &mut Sites, incoming: Sites) {
    for (key, mut site) in incoming {
        match sites.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(site);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                existing.scopes.append(&mut site.scopes);
                if !site.is_log {
                    // Match the former sequential scan: later tracing metadata
                    // replaces location evidence, but never erases a prior skip
                    // reason with the pending source-lookup placeholder.
                    existing.source = site.source;
                    existing.kind = site.kind;
                    existing.declaration = site.declaration;
                    if site
                        .template
                        .as_ref()
                        .is_err_and(|error| error.error != SOURCE_NOT_EXAMINED)
                    {
                        existing.template = site.template;
                    }
                }
            }
        }
    }
}

// Strings may be inline or referenced through a separate DWARF string section.
fn dwarf_string<R: gimli::Reader>(
    unit: gimli::UnitRef<'_, R>,
    value: Attr<R>,
) -> gimli::Result<String> {
    Ok(unit.attr_string(value)?.to_string_lossy()?.into_owned())
}

pub(super) fn dwarf_name<R: gimli::Reader>(
    unit: gimli::UnitRef<'_, R>,
    entry: &gimli::DebuggingInformationEntry<R>,
) -> gimli::Result<Option<String>> {
    entry
        .attr_value(gimli::DW_AT_name)
        .map(|value| dwarf_string(unit, value))
        .transpose()
}

// Most units contain no logging markers. Inspect only variable names and the
// root log::Level type before paying for function/source/namespace scope maps.
// Raw traversal still visits every DIE, including variables nested in closures
// and inline functions; unrelated attributes can be skipped in bulk by gimli.
struct Candidates<O> {
    tracing: bool,
    level_types: BTreeSet<gimli::UnitOffset<O>>,
}

fn scan_candidates<R: gimli::Reader>(unit: gimli::UnitRef<'_, R>) -> Result<Candidates<R::Offset>> {
    let mut found = Candidates {
        tracing: false,
        level_types: BTreeSet::new(),
    };
    let mut has_lvl = false;
    let mut in_log = false;
    let mut entries = unit.entries_raw(None)?;
    let mut attrs = Vec::new();
    while !entries.is_empty() {
        let offset = entries.next_offset();
        let depth = entries.next_depth();
        let Some(abbrev) = entries.read_abbreviation()? else {
            continue;
        };
        if depth == 1 {
            in_log = false;
        }
        let root_namespace = depth == 1 && abbrev.tag() == gimli::DW_TAG_namespace;
        let level_enum = in_log && depth == 2 && abbrev.tag() == gimli::DW_TAG_enumeration_type;
        if abbrev.tag() != gimli::DW_TAG_variable && !root_namespace && !level_enum {
            entries.skip_attributes(abbrev.attributes())?;
            continue;
        }
        entries.read_attributes(abbrev.attributes(), &mut attrs)?;
        let Some(name) = attrs.iter().find(|attr| attr.name() == gimli::DW_AT_name) else {
            continue;
        };
        let name = unit.attr_string(name.value())?;
        let name = name.to_string_lossy()?;
        if root_namespace {
            in_log = name == "log";
        } else if level_enum {
            if name == "Level" {
                found.level_types.insert(offset);
            }
        } else {
            found.tracing |= name == "__CALLSITE";
            has_lvl |= name == "lvl";
        }
    }
    // The enum alone is common in dependencies that never emit log records.
    if !has_lvl {
        found.level_types.clear();
    }
    Ok(found)
}

// Rust DWARF also uses namespaces for function-local statics and impl types.
// Track lexical namespaces, then exclude scopes identified by function/type DIEs.
#[derive(Clone, Default)]
pub(super) struct Namespaces(pub(super) Vec<(isize, String)>);

impl Namespaces {
    fn visit<R: gimli::Reader>(
        &mut self,
        unit: gimli::UnitRef<'_, R>,
        entry: &gimli::DebuggingInformationEntry<R>,
    ) -> gimli::Result<()> {
        while self
            .0
            .last()
            .is_some_and(|(depth, _)| *depth >= entry.depth())
        {
            self.0.pop();
        }
        if entry.tag() == gimli::DW_TAG_namespace {
            self.0
                .push((entry.depth(), dwarf_name(unit, entry)?.unwrap_or_default()));
        }
        Ok(())
    }

    pub(super) fn module_path(&self, non_modules: &BTreeSet<String>) -> Option<String> {
        let mut path = String::new();
        for (_, name) in &self.0 {
            if name.is_empty() {
                return None;
            }
            let next = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}::{name}")
            };
            // Impl namespaces are explicit non-module boundaries. For closures,
            // an unidentified enclosing function makes the module ambiguous.
            if name.starts_with("{impl#") || non_modules.contains(&next) {
                break;
            }
            if name.starts_with('{') {
                return None;
            }
            path = next;
        }
        (!path.is_empty()).then_some(path)
    }
}

struct DwarfScopes<O> {
    non_modules: BTreeSet<String>,
    functions: BTreeMap<gimli::UnitOffset<O>, FunctionScope<O>>,
    namespace_functions: BTreeMap<String, BTreeSet<SourceLocation>>,
}

struct FunctionScope<O> {
    name: Option<String>,
    namespaces: Namespaces,
    reference: Option<gimli::UnitOffset<O>>,
    source: Option<SourceLocation>,
}

// Concrete/inline function DIEs may name their declaration instead of repeating
// its namespace. Follow only DWARF references, with a bound for malformed cycles.
impl<O: Copy + Ord> DwarfScopes<O> {
    fn function_module(&self, mut offset: gimli::UnitOffset<O>) -> Option<String> {
        for _ in 0..self.functions.len() {
            let function = self.functions.get(&offset)?;
            if let Some(path) = function.namespaces.module_path(&self.non_modules) {
                return Some(path);
            }
            offset = function.reference?;
        }
        None
    }

    fn function_source(&self, mut offset: gimli::UnitOffset<O>) -> Option<SourceLocation> {
        for _ in 0..self.functions.len() {
            let function = self.functions.get(&offset)?;
            if let Some(source) = &function.source {
                return Some(source.clone());
            }
            offset = function.reference?;
        }
        None
    }
}

fn scope_reference<R: gimli::Reader>(
    entry: &gimli::DebuggingInformationEntry<R>,
) -> Option<gimli::UnitOffset<R::Offset>> {
    match entry
        .attr_value(gimli::DW_AT_specification)
        .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin))
    {
        Some(Attr::UnitRef(offset)) => Some(offset),
        _ => None,
    }
}

fn visit_scope<R: gimli::Reader>(
    namespaces: &mut Namespaces,
    unit: gimli::UnitRef<'_, R>,
    entry: &gimli::DebuggingInformationEntry<R>,
    scopes: &DwarfScopes<R::Offset>,
) -> gimli::Result<()> {
    namespaces.visit(unit, entry)?;
    if namespaces.0.is_empty()
        && matches!(
            entry.tag(),
            gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine
        )
        && let Some(reference) = scope_reference(entry)
        && let Some(module) = scopes.function_module(reference)
    {
        namespaces.0.push((entry.depth(), module));
    }
    Ok(())
}

fn dwarf_scopes<R: gimli::Reader>(unit: gimli::UnitRef<'_, R>) -> Result<DwarfScopes<R::Offset>> {
    let mut scopes = Namespaces::default();
    let mut excluded = BTreeSet::new();
    let mut functions = BTreeMap::new();
    let mut entries = unit.entries();
    while let Some(entry) = entries.next_dfs()? {
        scopes.visit(unit, entry)?;
        // Linkers can discard a function but retain its callsite's DWARF. The
        // linkage name still distinguishes value scopes from module namespaces.
        if let Some(value) = entry.attr_value(gimli::DW_AT_linkage_name)
            && let Some(scope) = symbol_value_scope(&dwarf_string(unit, value)?)
        {
            excluded.insert(scope);
        }
        if entry.tag() == gimli::DW_TAG_subprogram {
            functions.insert(
                entry.offset(),
                FunctionScope {
                    name: dwarf_name(unit, entry)?,
                    namespaces: scopes.clone(),
                    reference: scope_reference(entry),
                    source: dwarf_location(unit, entry)?,
                },
            );
        }
        if matches!(
            entry.tag(),
            gimli::DW_TAG_subprogram
                | gimli::DW_TAG_structure_type
                | gimli::DW_TAG_enumeration_type
                | gimli::DW_TAG_union_type
        ) && let Some(name) = dwarf_name(unit, entry)?
        {
            let prefix = scopes
                .0
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join("::");
            // Monomorphized function/type names can include generic arguments.
            excluded.insert(format!("{prefix}::{}", name.split('<').next().unwrap()));
            excluded.insert(format!("{prefix}::{name}"));
        }
    }
    let mut metadata = DwarfScopes {
        non_modules: excluded,
        functions,
        namespace_functions: BTreeMap::new(),
    };
    // Tracing statics can live in synthetic function namespaces rather than
    // inside function DIEs. Resolve those exact namespace paths back to symbols.
    for (offset, function) in &metadata.functions {
        if let (Some(name), Some(source)) = (&function.name, metadata.function_source(*offset)) {
            let prefix = function
                .namespaces
                .0
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join("::");
            let path = format!("{prefix}::{}", name.split('<').next().unwrap());
            metadata
                .namespace_functions
                .entry(path)
                .or_default()
                .insert(source);
        }
    }
    Ok(metadata)
}

// Read only the crate/nested-path prefix of rustc v0 linkage names. `t` marks
// type/module namespaces; the first `v` marks a function/static, not a module.
// Unsupported impl/generic/backref/punycode forms return None, never a guess.
// Grammar: https://doc.rust-lang.org/rustc/symbol-mangling/v0.html
pub(super) fn symbol_value_scope(symbol: &str) -> Option<String> {
    fn identifier<'a>(input: &mut &'a str) -> Option<&'a str> {
        if let Some(rest) = input.strip_prefix('s') {
            let end = rest.find('_')?;
            if !rest[..end].bytes().all(|b| b.is_ascii_alphanumeric()) {
                return None;
            }
            *input = &rest[end + 1..];
        }
        let digits = input.bytes().take_while(u8::is_ascii_digit).count();
        let length: usize = input.get(..digits)?.parse().ok()?;
        *input = &input[digits..];
        *input = input.strip_prefix('_').unwrap_or(input);
        let name = input.get(..length)?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        *input = &input[length..];
        Some(name)
    }
    let mut input = symbol.strip_prefix("_R")?;
    let mut namespaces = Vec::new();
    while input.starts_with('N') {
        let namespace = *input.as_bytes().get(1)?;
        if !namespace.is_ascii_alphabetic() {
            return None;
        }
        namespaces.push(namespace);
        input = &input[2..];
    }
    input = input.strip_prefix('C')?;
    let mut path = vec![identifier(&mut input)?];
    for namespace in namespaces.into_iter().rev() {
        if !matches!(namespace, b't' | b'v') {
            return None;
        }
        path.push(identifier(&mut input)?);
        if namespace == b'v' {
            return Some(path.join("::"));
        }
    }
    None
}

fn dwarf_location<R: gimli::Reader>(
    unit: gimli::UnitRef<'_, R>,
    entry: &gimli::DebuggingInformationEntry<R>,
) -> Result<Option<SourceLocation>> {
    // Declaration file indexes the unit's line-program file table; line is direct.
    // Incomplete entries are skipped; malformed DWARF still propagates errors.
    let Some(Attr::FileIndex(index)) = entry.attr_value(gimli::DW_AT_decl_file) else {
        return Ok(None);
    };
    let Some(line) = entry
        .attr_value(gimli::DW_AT_decl_line)
        .and_then(|v| v.udata_value())
    else {
        return Ok(None);
    };
    let Some(program) = &unit.line_program else {
        return Ok(None);
    };
    let header = program.header();
    let Some(file) = header.file(index) else {
        return Ok(None);
    };
    let comp_dir = unit
        .comp_dir
        .as_ref()
        .map(|s| s.to_string_lossy().map(|s| PathBuf::from(s.as_ref())))
        .transpose()?
        .unwrap_or_default();
    let directory = file
        .directory(header)
        .map(|v| dwarf_string(unit, v))
        .transpose()?
        .unwrap_or_default();
    // Absolute components replace earlier parts. Preserve the resulting path in
    // the table: stripping comp_dir would collapse unrelated crates to src/lib.rs.
    let source = comp_dir
        .join(directory)
        .join(dwarf_string(unit, file.path_name())?);
    Ok(Some(SourceLocation {
        file: source.to_string_lossy().into_owned(),
        line: u32::try_from(line)?,
    }))
}

fn record_site<R: gimli::Reader>(
    unit: gimli::UnitRef<'_, R>,
    entry: &gimli::DebuggingInformationEntry<R>,
    is_log: bool,
    module_path: Option<String>,
    context: Option<SourceLocation>,
    sites: &mut Sites,
) -> Result<()> {
    let Some(SourceLocation { file, line }) = dwarf_location(unit, entry)? else {
        return Ok(());
    };
    let site = sites
        .entry((file.clone(), line, is_log, module_path.clone()))
        .or_insert_with(|| Callsite::unresolved(file, line, is_log, module_path));
    site.scopes.extend(context);
    Ok(())
}

// ---- Tracing DWARF matcher -------------------------------------------------

fn extract_tracing<R: gimli::Reader>(
    unit: gimli::UnitRef<'_, R>,
    metadata: &DwarfScopes<R::Offset>,
    memory: &static_metadata::Memory<'_>,
    sites: &mut Sites,
) -> Result<()> {
    let mut scopes = Namespaces::default();
    let mut decoder = static_metadata::Decoder::new(unit);
    let mut entries = unit.entries();
    while let Some(entry) = entries.next_dfs()? {
        visit_scope(&mut scopes, unit, entry, metadata)?;
        // Tracing generates a static __CALLSITE with type DefaultCallsite.
        if entry.tag() == gimli::DW_TAG_variable
            && dwarf_name(unit, entry)?.as_deref() == Some("__CALLSITE")
            && decoder.is_callsite(entry)?
        {
            // The declaration identifies the object and source evidence only.
            // Runtime identity comes entirely from __CALLSITE.meta's typed data.
            let Some(static_metadata::Metadata {
                file,
                line,
                module_path,
                kind,
                has_message,
            }) = decoder.read_callsite(memory, entry).map_err(|error| {
                format!(
                    "cannot decode tracing __CALLSITE at DWARF {:?}: {error}",
                    entry.offset()
                )
            })?
            else {
                continue;
            };
            let source = unit
                .comp_dir
                .as_ref()
                .map(|dir| dir.to_string_lossy().map(|dir| PathBuf::from(dir.as_ref())))
                .transpose()?
                .unwrap_or_default()
                .join(&file);
            let declaration = dwarf_location(unit, entry)?;
            let path = scopes
                .0
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join("::");
            let site = sites
                .entry((file.clone(), line, false, module_path.clone()))
                .or_insert_with(|| Callsite::unresolved(file, line, false, module_path));
            site.source = source;
            site.kind = kind;
            if kind == CallsiteKind::Span || !has_message {
                site.template = Err(LookupError {
                    line: String::new(),
                    error: if kind == CallsiteKind::Span {
                        SPAN_TEMPLATE
                    } else {
                        NO_MESSAGE
                    }
                    .into(),
                });
            }
            site.declaration = declaration;
            if let Some(contexts) = metadata.namespace_functions.get(&path) {
                site.scopes.extend(contexts.iter().cloned());
            }
        }
    }
    Ok(())
}

// ---- Log DWARF matcher -----------------------------------------------------

fn extract_log<R: gimli::Reader>(
    unit: gimli::UnitRef<'_, R>,
    metadata: &DwarfScopes<R::Offset>,
    level_types: &BTreeSet<gimli::UnitOffset<R::Offset>>,
    sites: &mut Sites,
) -> Result<()> {
    // Unlike tracing's statics, log 0.4 macros generate a local `lvl: log::Level`.
    // Resolve Level within the root log namespace to reject unrelated Level enums.
    // Candidate discovery already resolved the Level offsets in this unit.
    let mut scopes = Namespaces::default();
    let mut functions = Vec::new();
    let mut entries = unit.entries();
    while let Some(entry) = entries.next_dfs()? {
        visit_scope(&mut scopes, unit, entry, metadata)?;
        while functions
            .last()
            .is_some_and(|(depth, _)| *depth >= entry.depth())
        {
            functions.pop();
        }
        if matches!(
            entry.tag(),
            gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine
        ) {
            let offset = scope_reference(entry).unwrap_or(entry.offset());
            functions.push((entry.depth(), metadata.function_source(offset)));
        }
        if entry.tag() == gimli::DW_TAG_variable
            && dwarf_name(unit, entry)?.as_deref() == Some("lvl")
            && let Some(Attr::UnitRef(ty)) = entry.attr_value(gimli::DW_AT_type)
            && level_types.contains(&ty)
        {
            record_site(
                unit,
                entry,
                true,
                scopes.module_path(&metadata.non_modules),
                functions
                    .iter()
                    .rev()
                    .find_map(|(_, source)| source.clone()),
                sites,
            )?;
        }
    }
    Ok(())
}
