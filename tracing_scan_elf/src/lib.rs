//! Scan tracing callsites and load their CSV or embedded ELF metadata.
//! Artifact readers need neither DWARF nor the original source tree.

use gimli::AttributeValue as Attr;
use object::{Object, ObjectSection};
use proc_macro2::{TokenStream, TokenTree as Token};
use quote::ToTokens;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use syn::{Token as T, parse::Parser as _};

// ---- Shared types ----------------------------------------------------------

/// I/O, DWARF, and format errors; individual template failures live in `Callsite`.
pub type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

/// A literal format string plus Rust expressions with normalized token spacing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub format: String,
    pub args: Vec<String>,
    pub fields: Vec<String>,
}

/// A source lookup failure retained as data rather than aborting the scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupError {
    /// Original source line, when available (not its line number).
    pub line: String,
    pub error: String,
}

/// One tracing location, keyed by the build's source filename and line number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callsite {
    pub file: String,
    pub line: u32,
    /// Path used during extraction. Artifacts store only `file`, so readers
    /// initialize this to that filename; the original build path is unavailable.
    pub source: PathBuf,
    pub template: Result<Template, LookupError>,
}

impl Callsite {
    // Both readers reconstruct the same source-path fallback from stored data.
    fn loaded(file: String, line: u32, template: Result<Template, LookupError>) -> Self {
        Self {
            source: PathBuf::from(&file),
            file,
            line,
            template,
        }
    }
}

// ---- DWARF location extraction ---------------------------------------------

/// Find tracing callsite locations recorded in an ELF's DWARF debug information.
/// This stage only identifies source locations; template lookup fills in the Rust
/// source details later, independently of the eventual output format.
pub fn extract(elf: &object::File<'_>) -> Result<Vec<Callsite>> {
    // Require a linked image rather than an intermediate relocatable object.
    if elf.format() != object::BinaryFormat::Elf || elf.kind() == object::ObjectKind::Relocatable {
        return Err("expected a linked ELF executable or shared library".into());
    }
    // Load and decompress each DWARF section, treating absent sections as empty.
    // Keep this backing storage alive while gimli borrows endian-aware readers.
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
    // A source callsite can appear in multiple compilation units or generic
    // instantiations. Key by file and line to deduplicate and sort the result.
    let mut sites = BTreeMap::new();
    let mut units = dwarf.units();
    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        // Strings may be inline or referenced through a DWARF string section;
        // resolve them in this unit's context and return owned, lossy UTF-8.
        let string = |value| -> gimli::Result<String> {
            Ok(dwarf
                .attr_string(&unit, value)?
                .to_string_lossy()
                .into_owned())
        };
        let comp_dir = unit
            .comp_dir
            .as_ref()
            .map(|s| PathBuf::from(s.to_string_lossy().as_ref()))
            .unwrap_or_default();
        let mut entries = unit.entries();
        // Walk nested debug information entries (DIEs), including variables
        // inside function and namespace scopes. Skip entries missing metadata
        // needed below; malformed DWARF reads still propagate errors via `?`.
        while let Some(entry) = entries.next_dfs()? {
            if entry.tag() != gimli::DW_TAG_variable {
                continue;
            }
            let Some(name) = entry.attr_value(gimli::DW_AT_name) else {
                continue;
            };
            if string(name)? != "__CALLSITE" {
                continue;
            }
            // Match both the macro-generated variable name and its type name.
            // This follows a type reference within the current compilation unit.
            let Some(Attr::UnitRef(ty)) = entry.attr_value(gimli::DW_AT_type) else {
                continue;
            };
            let ty = unit.entry(ty)?;
            let Some(name) = ty.attr_value(gimli::DW_AT_name) else {
                continue;
            };
            if string(name)? != "DefaultCallsite" {
                continue;
            }
            // The declaration's file attribute indexes this unit's line-program
            // file table; the declaration line supplies the source line directly.
            let Some(Attr::FileIndex(index)) = entry.attr_value(gimli::DW_AT_decl_file) else {
                continue;
            };
            let Some(line) = entry
                .attr_value(gimli::DW_AT_decl_line)
                .and_then(|v| v.udata_value())
            else {
                continue;
            };
            let Some(program) = &unit.line_program else {
                continue;
            };
            let header = program.header();
            let Some(file) = header.file(index) else {
                continue;
            };
            let directory = file
                .directory(header)
                .map(&string)
                .transpose()?
                .unwrap_or_default();
            // Reconstruct the source path from the compilation directory, file
            // directory, and filename. Path::join lets absolute parts take over.
            let source = comp_dir.join(directory).join(string(file.path_name())?);
            // Relative keys match file!() in ordinary Cargo builds and survive relinking.
            let file = source
                .strip_prefix(&comp_dir)
                .unwrap_or(&source)
                .to_string_lossy()
                .into_owned();
            let line = u32::try_from(line)?;
            sites.entry((file.clone(), line)).or_insert(Callsite {
                file,
                line,
                source,
                // Extraction does not open source files. The next stage replaces
                // this placeholder with a parsed template or a lookup error.
                template: Err(LookupError {
                    line: String::new(),
                    error: "source not examined".into(),
                }),
            });
        }
    }
    Ok(sites.into_values().collect())
}

// ---- Rust source and template lookup ---------------------------------------

// Parse expressions rather than splitting text: generics, closures, nested macros,
// raw strings, named format arguments, and tracing's ?/% fields retain their meaning.
fn parse_template(tokens: TokenStream, name: &str) -> syn::Result<Template> {
    let mut fields = Vec::new();
    (|input: syn::parse::ParseStream| {
        let mut level = matches!(name, "event" | "span");
        let mut message = None;
        while !input.is_empty() {
            if input.peek(syn::LitStr) && !input.peek2(T![=]) {
                let format = input.parse::<syn::LitStr>()?.value();
                if !input.is_empty() {
                    input.parse::<T![,]>()?;
                }
                if name == "span" || name.ends_with("_span") {
                    while !input.is_empty() {
                        fields.push(parse_field(input)?.0);
                        if !input.is_empty() {
                            input.parse::<T![,]>()?;
                        }
                    }
                    return Ok(Template {
                        format,
                        args: vec![],
                        fields: fields.clone(),
                    });
                }
                let args =
                    syn::punctuated::Punctuated::<syn::Expr, T![,]>::parse_terminated(input)?;
                return Ok(Template {
                    format,
                    args: args
                        .iter()
                        .map(|e| e.to_token_stream().to_string())
                        .collect(),
                    fields: fields.clone(),
                });
            }
            if input.peek(syn::Ident) && input.peek2(T![:]) && !input.peek3(T![:]) {
                input.parse::<syn::Ident>()?;
                input.parse::<T![:]>()?;
                input.parse::<syn::Expr>()?;
            } else if level {
                input.parse::<syn::Expr>()?;
                level = false;
            } else if input.peek(syn::token::Brace) {
                let content;
                syn::braced!(content in input);
                while !content.is_empty() {
                    let (field, template) = parse_field(&content)?;
                    fields.push(field);
                    message = template.or(message);
                    if !content.is_empty() {
                        content.parse::<T![,]>()?;
                    }
                }
            } else {
                let (field, template) = parse_field(input)?;
                fields.push(field);
                message = template.or(message);
            }
            if !input.is_empty() {
                input.parse::<T![,]>()?;
            }
        }
        match message {
            Some(mut template) => {
                template.fields = fields.clone();
                Ok(template)
            }
            None => Err(input.error("no literal format string in macro")),
        }
    })
    .parse2(tokens)
}

fn parse_field(input: syn::parse::ParseStream) -> syn::Result<(String, Option<Template>)> {
    let mut field = TokenStream::new();
    if input.peek(T![?]) || input.peek(T![%]) {
        field.extend([input.parse::<Token>()?]);
    }
    let key: Token = input.parse()?;
    let message = key.to_string() == "message";
    field.extend([key]);
    while input.peek(T![.]) {
        field.extend([input.parse::<Token>()?, input.parse::<Token>()?]);
    }
    let mut template = None;
    if input.peek(T![=]) {
        field.extend([input.parse::<Token>()?]);
        if input.peek(T![?]) || input.peek(T![%]) {
            field.extend([input.parse::<Token>()?]);
        }
        let value: syn::Expr = input.parse()?;
        if message {
            template = match &value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(Template {
                    format: s.value(),
                    args: vec![],
                    fields: vec![],
                }),
                syn::Expr::Macro(m) => Some(parse_template(
                    m.mac.tokens.clone(),
                    &m.mac.path.to_token_stream().to_string(),
                )?),
                _ => None,
            };
        }
        value.to_tokens(&mut field);
    }
    Ok((field.to_string(), template))
}

fn source_templates(tokens: TokenStream, found: &mut BTreeMap<u32, Vec<Result<Template, String>>>) {
    let tokens: Vec<_> = tokens.into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        i += 1;
        // Do not recurse into a macro's arguments: nested format! belongs to it.
        if let (Token::Ident(name), Some(Token::Punct(bang)), Some(Token::Group(args))) =
            (token, tokens.get(i), tokens.get(i + 1))
            && bang.as_char() == '!'
        {
            found
                .entry(token.span().start().line as u32)
                .or_default()
                .push(parse_template(args.stream(), &name.to_string()).map_err(|e| e.to_string()));
            i += 2;
            continue;
        }
        if let Token::Group(group) = token {
            source_templates(group.stream(), found);
        }
    }
}

/// Read each source file once and resolve templates at the extracted locations.
/// Use `source_root` to relocate the build-relative filenames to another tree.
pub fn lookup_templates(sites: &mut [Callsite], source_root: Option<&Path>) {
    let mut files = BTreeMap::new();
    for site in sites {
        let (source, templates) = files.entry(site.source.clone()).or_insert_with(|| {
            let path =
                source_root.map_or_else(|| site.source.clone(), |root| root.join(&site.file));
            let mut found = BTreeMap::new();
            let source = fs::read_to_string(&path).map_err(|e| e.to_string());
            if let Ok(source) = &source
                && let Ok(tokens) = source.parse::<TokenStream>()
            {
                source_templates(tokens, &mut found);
            }
            (source, found)
        });
        // Two macros on one line cannot be distinguished by a file + line key.
        let result = match templates.get(&site.line).map(Vec::as_slice) {
            Some([result]) => result.clone(),
            Some(_) => Err("ambiguous macros on one line".into()),
            None => Err(source
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "no macro with a format string at DWARF location".into())),
        };
        site.template = result.map_err(|error| LookupError {
            error,
            line: source
                .as_ref()
                .ok()
                .and_then(|s| s.lines().nth(site.line.saturating_sub(1) as usize))
                .unwrap_or_default()
                .into(),
        });
    }
}

// ---- CSV reading and writing -----------------------------------------------

const COLUMNS: [&str; 8] = [
    "file", "line", "result", "format", "args", "fields", "source", "error",
];

/// Load a scanner CSV file. Malformed rows fail the load; stored lookup errors
/// remain `Err` values inside individual callsites.
pub fn load_csv(path: impl AsRef<Path>) -> Result<Vec<Callsite>> {
    read_csv(fs::File::open(path)?)
}

/// Parse a scanner CSV stream, including quoted commas, quotes, and newlines.
pub fn read_csv(reader: impl Read) -> Result<Vec<Callsite>> {
    let mut reader = ::csv::Reader::from_reader(reader);
    if !reader.headers()?.iter().eq(COLUMNS) {
        return Err(format!("expected CSV columns: {}", COLUMNS.join(",")).into());
    }
    reader
        .deserialize()
        .map(|row| {
            let row: Row = row?;
            let template = match row.result {
                Status::Ok => Ok(Template {
                    format: row.format,
                    args: serde_json::from_str(&row.args)?,
                    fields: serde_json::from_str(&row.fields)?,
                }),
                Status::Err => Err(LookupError {
                    line: row.source,
                    error: row.error,
                }),
            };
            Ok(Callsite::loaded(row.file, row.line, template))
        })
        .collect()
}

// Deserialize the flat CSV schema first, then decode its nested JSON arrays.
#[derive(Deserialize)]
struct Row {
    file: String,
    line: u32,
    result: Status,
    format: String,
    args: String,
    fields: String,
    source: String,
    error: String,
}

#[derive(Deserialize)]
enum Status {
    Ok,
    Err,
}

/// Write the scanner CSV, including per-callsite lookup failures.
pub fn write_csv(sites: &[Callsite], path: &Path) -> Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(fs::File::create(path)?);
    writeln!(out, "{}", COLUMNS.join(","))?;
    for site in sites {
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let columns = match &site.template {
            Ok(t) => vec![
                "Ok".into(),
                t.format.clone(),
                serde_json::to_string(&t.args)?,
                serde_json::to_string(&t.fields)?,
                String::new(),
                String::new(),
            ],
            Err(e) => vec![
                "Err".into(),
                String::new(),
                String::new(),
                String::new(),
                e.line.clone(),
                e.error.clone(),
            ],
        };
        writeln!(
            out,
            "{},{},{}",
            quote(&site.file),
            site.line,
            columns
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(",")
        )?;
    }
    out.flush()?;
    Ok(())
}

// ---- Embedded ELF section reading and writing ------------------------------

/// ELF section name shared by artifact writers and post-processing tools.
pub const SECTION_NAME: &str = ".tracing_scan_elf";
const MAGIC: &[u8; 8] = b"TSCAN\0\x01\0";
const HEADER_BYTES: usize = 12;
const RECORD_BYTES: usize = 20;

/// Load the section added to this program's executable by post-processing.
/// Reads the executable file, so the section need not be mapped into memory or
/// expose linker symbols. No DWARF or source files are needed at runtime.
pub fn load_embedded_section() -> Result<Vec<Callsite>> {
    load_elf_section(std::env::current_exe()?)
}

/// Load `.tracing_scan_elf` from an ELF executable, shared library, or object file.
pub fn load_elf_section(path: impl AsRef<Path>) -> Result<Vec<Callsite>> {
    let bytes = fs::read(path)?;
    let elf = object::File::parse(bytes.as_slice())?;
    if elf.format() != object::BinaryFormat::Elf {
        return Err("expected an ELF file".into());
    }
    let section = elf
        .section_by_name(SECTION_NAME)
        .ok_or("ELF has no .tracing_scan_elf section")?;
    read_section(&section.uncompressed_data()?)
}

/// Decode raw section bytes (not an entire ELF file).
/// Check the version, record table, offsets, UTF-8, and JSON before returning data.
pub fn read_section(data: &[u8]) -> Result<Vec<Callsite>> {
    let header = data
        .get(..HEADER_BYTES)
        .ok_or("truncated tracing section header")?;
    if &header[..8] != MAGIC {
        return Err("invalid tracing section magic or unsupported version".into());
    }
    let count = usize::try_from(word(&header[8..12]))?;
    let table_end = count
        .checked_mul(RECORD_BYTES)
        .and_then(|size| HEADER_BYTES.checked_add(size))
        .ok_or("tracing section record count overflow")?;
    let table = data
        .get(HEADER_BYTES..table_end)
        .ok_or("truncated tracing section records")?;

    // Validate the table before allocating; corrupt counts cannot request an
    // allocation larger than the section's actual record table.
    let mut sites = Vec::with_capacity(count);
    for record in table.chunks_exact(RECORD_BYTES) {
        let file = payload(data, table_end, word(&record[..4]), word(&record[4..8]))?;
        let result = payload(
            data,
            table_end,
            word(&record[12..16]),
            word(&record[16..20]),
        )?;
        sites.push(Callsite::loaded(
            std::str::from_utf8(file)?.to_owned(),
            word(&record[8..12]),
            serde_json::from_slice(result)?,
        ));
    }
    Ok(sites)
}

// Callers provide four-byte fields from the already validated header/table.
fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

// Offsets are section-relative and may only reference bytes after the table.
fn payload(data: &[u8], table_end: usize, offset: u32, length: u32) -> Result<&[u8]> {
    let start = usize::try_from(offset)?;
    let end = start
        .checked_add(usize::try_from(length)?)
        .ok_or("tracing section range overflow")?;
    if start < table_end {
        return Err("tracing section payload overlaps its record table".into());
    }
    data.get(start..end)
        .ok_or_else(|| "tracing section payload is out of bounds".into())
}

/// Write a relocatable ELF object containing `.tracing_scan_elf`.
/// The section's bytes can be added to the target executable in a later step.
pub fn write_section(sites: &[Callsite], elf: &object::File<'_>, path: &Path) -> Result<()> {
    let data = encode(sites)?;
    let mut out = object::write::Object::new(
        object::BinaryFormat::Elf,
        elf.architecture(),
        elf.endianness(),
    );
    out.flags = elf.flags();
    let section = out.add_section(
        Vec::new(),
        SECTION_NAME.as_bytes().to_vec(),
        object::SectionKind::ReadOnlyData,
    );
    let size = data.len() as u64;
    out.set_section_data(section, data, 4);
    // Retain boundary symbols for consumers that choose to link the object.
    // The post-processing reader above does not depend on these symbols.
    for (name, value) in [
        ("tracing_scan_elf_start", 0),
        ("tracing_scan_elf_end", size),
    ] {
        out.add_symbol(object::write::Symbol {
            name: name.as_bytes().to_vec(),
            value,
            size: 0,
            kind: object::SymbolKind::Data,
            scope: object::SymbolScope::Linkage,
            weak: false,
            section: object::write::SymbolSection::Section(section),
            flags: object::SymbolFlags::None,
        });
    }
    // Mark the object as not requiring an executable stack on ELF linkers.
    out.add_section(
        Vec::new(),
        b".note.GNU-stack".to_vec(),
        object::SectionKind::Other,
    );
    fs::write(path, out.write()?)?;
    Ok(())
}

fn encode(sites: &[Callsite]) -> Result<Vec<u8>> {
    // Relocation-free v1 layout: magic, LE u32 count, 20-byte records, strings.
    // Records contain file offset/length, line, and JSON result offset/length.
    let base = sites
        .len()
        .checked_mul(RECORD_BYTES)
        .and_then(|size| HEADER_BYTES.checked_add(size))
        .ok_or("tracing section record count overflow")?;
    let mut data = MAGIC.to_vec();
    data.extend(u32::try_from(sites.len())?.to_le_bytes());
    let mut strings = Vec::new();
    for site in sites {
        let file = u32::try_from(base + strings.len())?;
        strings.extend(site.file.as_bytes());
        let result = u32::try_from(base + strings.len())?;
        let payload = serde_json::to_vec(&site.template)?;
        let length = u32::try_from(payload.len())?;
        strings.extend(payload);
        for value in [
            file,
            u32::try_from(site.file.len())?,
            site.line,
            result,
            length,
        ] {
            data.extend(value.to_le_bytes());
        }
    }
    data.extend(strings);
    Ok(data)
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Exercise quoting and UTF-8 in both formats, including stored lookup errors.
    fn sample_sites() -> Vec<Callsite> {
        vec![
            Callsite::loaded(
                "src/é,\"file\".rs".into(),
                7,
                Ok(Template {
                    format: "hello,\r\n\"世界\" {}".into(),
                    args: vec!["(a, b)".into()],
                    fields: vec!["? user".into()],
                }),
            ),
            Callsite::loaded(
                "src/error.rs".into(),
                9,
                Err(LookupError {
                    line: "tracing::info!(\n  answer = \"a,b\"\n);".into(),
                    error: "no literal format string".into(),
                }),
            ),
        ]
    }

    #[test]
    fn artifacts_round_trip() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("tracing-scan-test-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let expected = sample_sites();
        let mut sites = expected.clone();
        // Build-only source paths are intentionally absent from both artifacts.
        sites[0].source = PathBuf::from("/original/build/src/file.rs");
        let csv = dir.join("templates.csv");
        write_csv(&sites, &csv)?;
        assert_eq!(load_csv(&csv)?, expected);
        assert_eq!(read_csv(fs::read(&csv)?.as_slice())?, expected);

        let bytes = fs::read(std::env::current_exe()?)?;
        let elf = object::File::parse(bytes.as_slice())?;
        let object_path = dir.join("templates.o");
        write_section(&sites, &elf, &object_path)?;
        assert_eq!(load_elf_section(&object_path)?, expected);
        let bytes = fs::read(&object_path)?;
        let object = object::File::parse(bytes.as_slice())?;
        let raw = object.section_by_name(SECTION_NAME).unwrap().data()?;
        assert_eq!(read_section(raw)?, expected);
        assert!(read_section(&encode(&[])?)?.is_empty());

        // A valid empty CSV consists of its header; an empty file is invalid.
        write_csv(&[], &csv)?;
        assert!(load_csv(&csv)?.is_empty());
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn rejects_invalid_csv() {
        let header = format!("{}\n", COLUMNS.join(","));
        for row in [
            "file.rs,1,Unknown,hello,[],[],,\n",
            "file.rs,-1,Ok,hello,[],[],,\n",
            "file.rs,1,Ok,hello,{},[],,\n",
            "file.rs,1,Ok,hello,,[],,\n",
            "file.rs,1,Ok\n",
        ] {
            assert!(
                read_csv(format!("{header}{row}").as_bytes()).is_err(),
                "{row}"
            );
        }
        assert!(read_csv(&b""[..]).is_err());
        assert!(read_csv(&b"file,line\nfile.rs,1\n"[..]).is_err());
    }

    #[test]
    fn rejects_invalid_sections() -> Result<()> {
        let valid = encode(&sample_sites())?;
        // Every truncated prefix must fail without panicking or accepting a
        // partial table. These records reference the final byte of the payload.
        for end in 0..valid.len() {
            assert!(read_section(&valid[..end]).is_err(), "prefix length {end}");
        }
        for at in [0, 6] {
            let mut corrupt = valid.clone();
            corrupt[at] ^= 0xff;
            assert!(read_section(&corrupt).is_err());
        }
        // Oversized count and out-of-bounds file/result offsets and lengths.
        for at in [8, 12, 16, 24, 28] {
            let mut corrupt = valid.clone();
            corrupt[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(read_section(&corrupt).is_err());
        }
        let mut corrupt = valid.clone();
        corrupt[12..16].copy_from_slice(&0u32.to_le_bytes());
        assert!(read_section(&corrupt).is_err()); // Filename overlaps the header.
        let mut corrupt = valid.clone();
        corrupt[HEADER_BYTES + 2 * RECORD_BYTES] = 0xff;
        assert!(read_section(&corrupt).is_err()); // Invalid UTF-8 filename.
        let mut corrupt = valid.clone();
        corrupt[word(&valid[24..28]) as usize] = b'!';
        assert!(read_section(&corrupt).is_err()); // Invalid JSON result.
        Ok(())
    }

    #[test]
    fn dwarf_to_templates() -> Result<()> {
        // Real callsites in this binary exercise DWARF and source lookup together.
        let good = line!() + 1;
        tracing::info!(answer = 42, "hello {}", 42);
        let bad = line!() + 1;
        tracing::info!(answer = 42);
        let bytes = fs::read(std::env::current_exe()?)?;
        let elf = object::File::parse(bytes.as_slice())?;
        let mut sites = extract(&elf)?;
        sites.retain(|site| {
            site.source.ends_with("tracing_scan_elf/src/lib.rs") && [good, bad].contains(&site.line)
        });
        assert_eq!(sites.len(), 2);
        lookup_templates(&mut sites, None);
        assert_eq!(sites[0].template.as_ref().unwrap().args, ["42"]);
        assert!(
            sites[1]
                .template
                .as_ref()
                .unwrap_err()
                .line
                .contains("tracing::info!")
        );
        Ok(())
    }

    #[test]
    fn formats_arguments_and_fields() {
        let t = parse_template(
            r##"target: "app", ?user, item.id = %id, "got {value:?}\n{}", make::<u8, u16>(), value = (1, 2)"##
                .parse()
                .unwrap(),
            "info",
        )
        .unwrap();
        assert_eq!(t.format, "got {value:?}\n{}");
        assert_eq!(t.args.len(), 2);
        assert_eq!(t.fields, ["? user", "item . id = % id"]);
        let t = parse_template(
            r##"tracing::Level::INFO, { answer = 42 }, r#"raw {}"#, value"##
                .parse()
                .unwrap(),
            "event",
        )
        .unwrap();
        assert_eq!(t.format, "raw {}");
        assert_eq!(t.fields, ["answer = 42"]);
        let t = parse_template(
            r#"message = %format_args!("hi {}", user), answer = 42"#
                .parse()
                .unwrap(),
            "info",
        )
        .unwrap();
        assert_eq!(t.args, ["user"]);
        assert_eq!(t.fields.len(), 2);
        assert!(parse_template("answer = 42".parse().unwrap(), "info").is_err());
    }
}
