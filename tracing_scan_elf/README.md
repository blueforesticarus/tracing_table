# tracing_scan_elf

Workspace package with two source files: `src/lib.rs` contains shared types,
extraction, template lookup, CSV/section readers and writers, and tests in clearly
marked sections. `src/main.rs` contains CLI options and orchestration.

## Build and run from the root

```sh
# Build tracing_table and tracing_scan_elf using the root Cargo.lock and target/.
cargo build

# Scan a linked ELF built with full DWARF (Cargo profile debug = 2).
cargo run -p tracing_scan_elf -- path/to/executable

# Run parser, format validation, round-trip, and real DWARF/source tests.
cargo test -p tracing_scan_elf
```

Produces `target/tracing_scan_elf/templates.csv` and
`target/tracing_scan_elf/templates.o` by default. Options: `-o DIR`,
`--format both|csv|section`, and `--source-root DIR` to relocate source paths.
The Bevy viewer remains a separate workspace.

## Use the library

The root package already has a path dependency on `tracing_scan_elf`. Other
packages can add the same dependency using the appropriate relative path.
The root library also reexports it as `tracing_table::tracing_scan_elf`.

```rust
use tracing_scan_elf::{Callsite, Result, load_embedded_section};

fn main() -> Result<()> {
    // Read metadata added to this executable by a post-processing step.
    let sites: Vec<Callsite> = load_embedded_section()?;
    for site in sites {
        println!("{}:{} {:?}", site.file, site.line, site.template);
    }
    Ok(())
}
```

Reader choices all return `Result<Vec<Callsite>>`:

| Function | Input |
| --- | --- |
| `load_embedded_section()` | `.tracing_scan_elf` in the running executable's file |
| `load_elf_section(path)` | Section in another ELF executable, library, or object |
| `read_section(bytes)` | Raw section bytes, without an ELF wrapper |
| `load_csv(path)` | Scanner CSV file |
| `read_csv(reader)` | CSV stream, including an in-memory byte slice |

The embedded reader opens `std::env::current_exe()` and reads the ELF section
table. This works for sections added after linking, without linker symbols or
mapped section memory. The executable must remain available on disk; readers
need neither DWARF nor the original Rust sources. Missing sections and malformed
artifacts return errors. There is no automatic CSV fallback.

`Template`, `LookupError`, and `Callsite` and their fields are public.
`site.template` preserves individual `Ok(Template)` / `Err(LookupError)` results.
Both formats store `file`, not the original build-machine source path, so loaded
callsites set `source` to `PathBuf::from(file)`.

## Extraction and formats

The scanner finds tracing's `__CALLSITE` / `DefaultCallsite` DWARF variables,
then reads the source macro at each file + line. It extracts decoded format
strings, argument expressions, and explicit tracing fields, preserving Rust
syntax with normalized token spacing. Raw strings, multiline invocations,
nested expressions, named arguments, and `message = format_args!(...)` are
supported. For spans, the span name is `format`; implicit captured arguments
remain in the format string, e.g. `{user}`.

Unsupported/missing templates, unavailable source, and multiple macros on one
line become per-callsite errors with the original source line when available.
Source files must match the build. Extraction never executes the input binary.

CSV columns, in order: `file,line,result,format,args,fields,source,error`.
`args` and `fields` contain JSON arrays; `result` is `Ok` or `Err`. The `source`
column holds the source **line text** for an error, not a source path. Quoted
commas, quotes, and newlines are supported.

The generated object matches the input ELF's architecture and byte order and
contains a read-only `.tracing_scan_elf` section. A post-processing step can copy
that section's raw bytes into the final executable. For example, with GNU objcopy:

```sh
# Extract the raw payload; do not add the entire templates.o as the section.
objcopy --dump-section .tracing_scan_elf=target/tracing_scan_elf/section.bin \
  target/tracing_scan_elf/templates.o
objcopy --add-section .tracing_scan_elf=target/tracing_scan_elf/section.bin \
  path/to/executable path/to/executable.with-templates
```

The payload is relocation-free. All integers are **little-endian u32**, regardless
of the ELF's byte order:

| Offset | Content |
| --- | --- |
| 0 | Eight magic/version bytes: `TSCAN\0\x01\0` |
| 8 | Record count |
| 12 | Records, 20 bytes each |
| After records | UTF-8 filenames and JSON result payloads |

Each record contains `(file_offset, file_length, line, result_offset, result_length)`.
Offsets are section-relative; strings have no terminator. Extraction sorts by
`(file, line)`; writers and readers preserve the supplied order. JSON payloads are
`{"Ok":{"format":"...","args":["..."],"fields":["..."]}}` or
`{"Err":{"line":"original source line","error":"reason"}}`.
