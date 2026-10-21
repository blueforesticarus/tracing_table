//! CSV and embedded-section formats, validation, and ELF artifact writers.
//! These readers do not require DWARF or the original source tree.

use super::source::decode_template;
use super::{Callsite, CallsiteKind, LookupError, Result, Template};
use object::{Object, ObjectSection};
use serde::{Deserialize, Serialize};
use std::{fs, io::Read, path::Path};

// ---- CSV reading and writing -----------------------------------------------

pub(super) const COLUMNS: [&str; 14] = [
    "file",
    "line",
    "result",
    "format",
    "args",
    "fields",
    "source",
    "error",
    "is_log",
    "module_path",
    "row_id",
    "wrapper_row",
    "type",
    "macro_call",
];

/// Load a scanner CSV file. Malformed rows fail the load; stored lookup errors
/// remain `Err` values inside individual callsites.
pub fn load_csv(path: impl AsRef<Path>) -> Result<Vec<Callsite>> {
    read_csv(fs::File::open(path)?)
}

/// Parse a scanner CSV stream, including quoted commas, quotes, and newlines.
pub fn read_csv(reader: impl Read) -> Result<Vec<Callsite>> {
    let mut reader = ::csv::Reader::from_reader(reader);
    // Older tables lack module_path (9 columns), or both new metadata fields (8).
    let headers = reader.headers()?;
    let linked_format = headers.len() >= 12;
    if ![8, 9, 10, 12, 13, 14]
        .into_iter()
        .any(|n| headers.iter().eq(COLUMNS[..n].iter().copied()))
    {
        return Err(format!("expected CSV columns: {}", COLUMNS.join(",")).into());
    }
    let sites: Vec<_> = reader
        .deserialize()
        .enumerate()
        .map(|(index, row)| {
            let row: Row = row?;
            if linked_format && row.row_id != Some(index) {
                return Err("CSV row_id must match its zero-based table index".into());
            }
            let template = match row.result {
                Status::Ok => Ok(decode_template(
                    row.format,
                    serde_json::from_str(&row.args)?,
                    serde_json::from_str(&row.fields)?,
                )?),
                Status::Err => Err(LookupError {
                    line: row.source.clone(),
                    error: row.error,
                }),
            };
            let mut site =
                Callsite::loaded(row.file, row.line, row.is_log, row.module_path, template);
            site.wrapper_row = row.wrapper_row;
            site.kind = row.kind.unwrap_or(site.kind);
            site.source_line = row.source;
            site.macro_call = row.macro_call.unwrap_or_else(|| site.source_line.clone());
            Ok(site)
        })
        .collect::<Result<_>>()?;
    validate_links(&sites)?;
    Ok(sites)
}

// Share the flat CSV schema between readers and writers; arrays are JSON strings.
#[derive(Default, Serialize, Deserialize)]
struct Row {
    file: String,
    line: u32,
    result: Status,
    format: String,
    args: String,
    fields: String,
    source: String,
    error: String,
    #[serde(default)]
    is_log: bool,
    #[serde(default)]
    module_path: Option<String>,
    #[serde(default)]
    row_id: Option<usize>,
    #[serde(default)]
    wrapper_row: Option<usize>,
    #[serde(default, rename = "type")]
    kind: Option<CallsiteKind>,
    #[serde(default)]
    macro_call: Option<String>,
}

// Links are foreign keys into this table, not CSV physical line numbers. Check
// before writing/after loading so invalid references never reach runtime users.
fn validate_links(sites: &[Callsite]) -> Result<()> {
    for (row, site) in sites.iter().enumerate() {
        if site.is_log != (site.kind == CallsiteKind::Log) {
            return Err("callsite type disagrees with is_log".into());
        }
        let mut next = site.wrapper_row;
        for depth in 0..=sites.len() {
            let Some(parent) = next else {
                break;
            };
            if parent >= sites.len() {
                return Err("wrapper_row is outside the table".into());
            }
            if parent == row || depth == sites.len() {
                return Err("cycle in wrapper_row links".into());
            }
            if sites[parent].is_log != site.is_log {
                return Err("wrapper_row has a different logging framework".into());
            }
            next = sites[parent].wrapper_row;
        }
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
enum Status {
    Ok,
    #[default]
    Err,
}

/// Write the scanner CSV, including per-callsite lookup failures.
pub fn write_csv(sites: &[Callsite], path: &Path) -> Result<()> {
    validate_links(sites)?;
    // Write headers explicitly so an empty table is still a valid scanner CSV.
    let mut out = csv::WriterBuilder::new()
        .has_headers(false)
        .from_path(path)?;
    out.write_record(COLUMNS)?;
    for (index, site) in sites.iter().enumerate() {
        let mut row = Row {
            file: site.file.clone(),
            line: site.line,
            is_log: site.is_log,
            module_path: site.module_path.clone(),
            row_id: Some(index),
            wrapper_row: site.wrapper_row,
            kind: Some(site.kind),
            source: site.source_line.clone(),
            macro_call: Some(site.macro_call.clone()),
            ..Row::default()
        };
        match &site.template {
            Ok(t) => {
                row.result = Status::Ok;
                row.format = t.format.clone();
                row.args = serde_json::to_string(&t.args)?;
                row.fields = serde_json::to_string(&t.fields)?;
            }
            Err(e) => {
                if row.source.is_empty() {
                    row.source = e.line.clone();
                }
                row.error = e.error.clone();
            }
        }
        out.serialize(row)?;
    }
    out.flush()?;
    Ok(())
}

// ---- Embedded ELF section reading and writing ------------------------------

/// ELF section name shared by artifact writers and post-processing tools.
pub const SECTION_NAME: &str = ".tracing_scan_elf";
const MAGIC: &[u8; 8] = b"TSCAN\0\x06\0";
pub(super) const HEADER_BYTES: usize = 12;
pub(super) const RECORD_BYTES: usize = 36;

// V6 keeps the relocation-free record layout; its JSON payload also carries
// classification and original source independently of the template result.
#[derive(Serialize, Deserialize)]
struct SectionData {
    #[serde(rename = "type")]
    kind: CallsiteKind,
    source_line: String,
    macro_call: String,
    template: Result<Template, LookupError>,
}

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
    // V6 adds source/classification in JSON; V5 already has the wrapper row word.
    let record_bytes = if &header[..8] == MAGIC || &header[..8] == b"TSCAN\0\x05\0" {
        RECORD_BYTES
    } else if &header[..8] == b"TSCAN\0\x03\0" || &header[..8] == b"TSCAN\0\x04\0" {
        32
    } else if &header[..8] == b"TSCAN\0\x02\0" {
        24
    } else if &header[..8] == b"TSCAN\0\x01\0" {
        20
    } else {
        return Err("invalid tracing section magic or unsupported version".into());
    };
    let count = usize::try_from(word(&header[8..12]))?;
    let table_end = count
        .checked_mul(record_bytes)
        .and_then(|size| HEADER_BYTES.checked_add(size))
        .ok_or("tracing section record count overflow")?;
    let table = data
        .get(HEADER_BYTES..table_end)
        .ok_or("truncated tracing section records")?;

    // Validate the table before allocating; corrupt counts cannot request an
    // allocation larger than the section's actual record table.
    let mut sites = Vec::with_capacity(count);
    for record in table.chunks_exact(record_bytes) {
        let is_log = if record_bytes >= 24 {
            match word(&record[20..24]) {
                0 => false,
                1 => true,
                _ => return Err("invalid is_log boolean in tracing section".into()),
            }
        } else {
            false
        };
        let file = payload(data, table_end, word(&record[..4]), word(&record[4..8]))?;
        let result = payload(
            data,
            table_end,
            word(&record[12..16]),
            word(&record[16..20]),
        )?;
        let module_path = if record_bytes >= 32 {
            let module = payload(
                data,
                table_end,
                word(&record[24..28]),
                word(&record[28..32]),
            )?;
            let module = std::str::from_utf8(module)?;
            (!module.is_empty()).then(|| module.to_owned())
        } else {
            None
        };
        let stored = if &header[..8] == MAGIC {
            Some(serde_json::from_slice::<SectionData>(result)?)
        } else {
            None
        };
        let template = match &stored {
            Some(stored) => stored.template.clone(),
            None => serde_json::from_slice(result)?,
        };
        let mut site = Callsite::loaded(
            std::str::from_utf8(file)?.to_owned(),
            word(&record[8..12]),
            is_log,
            module_path,
            template,
        );
        if let Some(stored) = stored {
            site.kind = stored.kind;
            site.source_line = stored.source_line;
            site.macro_call = stored.macro_call;
        }
        if record_bytes == RECORD_BYTES {
            let row = word(&record[32..36]);
            site.wrapper_row = if row == u32::MAX {
                None
            } else {
                Some(usize::try_from(row)?)
            };
        }
        sites.push(site);
    }
    validate_links(&sites)?;
    Ok(sites)
}

// Callers provide four-byte fields from the already validated header/table.
pub(super) fn word(bytes: &[u8]) -> u32 {
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

/// Write raw `.tracing_scan_elf` bytes for `objcopy --add-section`.
pub fn write_raw_section(sites: &[Callsite], path: &Path) -> Result<()> {
    // Share the encoding with the ELF writer so both artifacts stay identical.
    fs::write(path, encode(sites)?)?;
    Ok(())
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

pub(super) fn encode(sites: &[Callsite]) -> Result<Vec<u8>> {
    validate_links(sites)?;
    // Relocation-free v6: magic, LE u32 count, 36-byte records, then strings/JSON.
    // An empty module string represents unavailable metadata, never a path guess.
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
        let payload = serde_json::to_vec(&SectionData {
            kind: site.kind,
            source_line: site.source_line.clone(),
            macro_call: site.macro_call.clone(),
            template: site.template.clone(),
        })?;
        let length = u32::try_from(payload.len())?;
        strings.extend(payload);
        let module_offset = u32::try_from(base + strings.len())?;
        let module = site.module_path.as_deref().unwrap_or_default();
        strings.extend(module.as_bytes());
        for value in [
            file,
            u32::try_from(site.file.len())?,
            site.line,
            result,
            length,
            u32::from(site.is_log),
            module_offset,
            u32::try_from(module.len())?,
            site.wrapper_row
                .map(u32::try_from)
                .transpose()?
                .unwrap_or(u32::MAX),
        ] {
            data.extend(value.to_le_bytes());
        }
    }
    data.extend(strings);
    Ok(data)
}
