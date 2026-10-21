# tracing_scan_elf

`src/lib.rs` contains shared types and public reexports. Implementation is divided
by stage: `dwarf.rs` discovers callsites, `static_metadata.rs` decodes retained
objects, `source.rs` parses templates and recovers wrappers, and `artifacts.rs`
reads/writes CSV and embedded sections. `main.rs` handles CLI orchestration;
`tests.rs` contains parser, format, and end-to-end fixtures.

## Build and run from the root

```sh
# Build tracing_table and tracing_scan_elf using the root Cargo.lock and target/.
cargo build

# Scan a linked ELF built with full DWARF (Cargo profile debug = 2).
cargo run -p tracing_scan_elf -- path/to/executable

# Run parser, format validation, round-trip, and real DWARF/source tests.
cargo test -p tracing_scan_elf
```

Produces `templates.csv`, `templates.bin`, and `templates.o` in
`<nearest target ancestor>/tracing_scan_elf/` of the input executable by default.
For example, `other/project/target/debug/app` writes to
`other/project/target/tracing_scan_elf/`, regardless of the working directory.
Relative paths and symlinks are resolved before walking up; if there is no
`target` ancestor, pass `-o DIR` explicitly.
`--format section` produces both `.bin` and `.o`;
`--format csv` produces only CSV. Options: `-o DIR`,
`--format both|csv|section`, `--threads N`, and `--source-root DIR` to relocate source paths.
The Bevy viewer remains a separate workspace.

The scanner binary built by this workspace is `target/debug/tracing_scan_elf`
at the repository root. An older `tracing_scan_elf/target/debug/tracing_scan_elf`
from before workspace integration is not updated by a normal `cargo build`;
use the root binary or `cargo run -p tracing_scan_elf` to run the current code.

## Scan performance

Use an optimized scanner for large binaries; the **input** still needs full DWARF.
`--timings` prints the read, DWARF extraction, source lookup, and output durations
to stderr, without changing the generated artifacts.
The CLI maps the input read-only, prefaulting pages where supported to avoid
repeated random DWARF string faults. The input must remain unchanged throughout
the scan; do not rebuild or truncate it concurrently.

```sh
# Optimize the scanner; this does not rebuild the executable being inspected.
cargo run --release -p tracing_scan_elf -- path/to/executable --timings

# Limit scan workers explicitly when comparing throughput or sharing the machine.
cargo run --release -p tracing_scan_elf -- path/to/executable --threads 8 --timings
```

Extraction first checks variable names and `log::Level` types with a lightweight
DWARF walk. Units without logging markers skip scope construction and extraction;
other units run only the relevant framework's matcher. The walk still descends
into functions, closures, and inline scopes, and retained tracing objects still
undergo the full metadata/layout validation.

DWARF compilation units run in parallel through Rayon, sharing immutable ELF
data while keeping decoder caches private to each worker. Results merge in
original compilation-unit order before sorting by callsite identity, preserving
duplicate scope evidence, row order, and wrapper references. Worker errors are
reported in unit order too.

`--threads N` overrides the worker count; otherwise Rayon uses
`RAYON_NUM_THREADS` or the available CPUs. `--threads 1` is useful for debugging
and checking deterministic output. `--timings` also prints the worker count.
Library `extract` and `lookup_templates` calls use the current Rayon pool, so
callers can control the pool without changing the library API.

Source lookup reads/tokenizes source files in parallel, sharing each file's tokens
between frameworks and macro text capture. Tokens and ASTs stay on their creating
thread; workers return only owned text, line indexes, and parsed template results.
Row filtering, wrapper recovery, and link remapping remain ordered on the caller.
AST parsing is lazy and skipped for wrapper declarations in files without
`macro_rules` definitions. Caches live for one lookup call, so later calls observe
source edits.

On the existing 1.4 GiB Bevy viewer, three alternating release runs on
October 21, 2026 measured these median full-scan times:

| Configuration | Full scan (s) | Source lookup (s) |
| --- | ---: | ---: |
| Previous scanner, 24 DWARF workers and serial lookup | 1.066 | 0.586 |
| Current scanner, `--threads 1` | 2.658 | 0.482 |
| Current scanner, `--threads 4` | 1.150 | 0.318 |
| Current scanner, `--threads 8` | 0.840 | 0.294 |
| Current scanner, automatic count (24 workers here) | 0.735 | 0.262 |

Parallel source parsing was about 2.2× faster for lookup and 1.5× faster overall.
All 1,108 callsites and 1,036 successful templates were preserved. CSV, raw
section, and object output were byte-for-byte identical for every worker count.
These are repeated local runs, not cold-cache measurements; results depend on
the input and machine.

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

`parse_source_templates(source, is_log)` exposes the CLI's source parser as a
one-based line-to-template map, retaining ambiguous and unsupported invocations
as errors. `tracing_table::subscriber::template::lookup(file, line, is_log, module_path)` uses this
parser when an embedded template is unavailable. It discovers the source project
above the executable's `target` directory, independently of the working directory;
set `TRACING_TABLE_SOURCE_ROOT` for a custom/shared target directory. Absolute
source paths also work without a project root. Relative and absolute section
paths are matched through canonical file identity when source is available;
unavailable source paths retain exact path matching, without suffix guessing.

The subscriber owns `Option<CompiledTemplate>` in `PerCallsiteData`; the compiled
matcher is freed with the callsite instead of leaked.
Source fallback results are intentionally leaked; the subscriber caches each
callsite's result, including failures. Parsed source files are shared between
callsites. Direct callers should also look up each callsite once and retain its
result. A missing entry in an existing section warns directly to stderr; failure
to recover a template from either section or source emits another warning.
Stored extraction failures count as present rows. Ambiguous locations remain
unresolved; module and logging-framework metadata disambiguate section entries.

Reverse matching is isolated at the bottom of `src/subscriber/template.rs`.
`CompiledTemplate::compile(format)` retains capture names in `field_names` and
merges adjacent placeholders into one anonymous capture (named by its zero-based
output slot). Other positional names follow Rust's argument indices; named and
repeated placeholders retain their names. `captures(message)` returns borrowed
rendered strings in that order, matching the entire message. Ambiguous internal
delimiters split at their first occurrence; the final literal is anchored to the
end. The compiled `format` reflects merged placeholders. Recording/archetype
storage does not yet execute this matcher.

`Template`, `LookupError`, and `Callsite` and their fields are public.
`Callsite::kind` is `Event`, `Span`, or `Log` (serialized as `event`, `span`, `log`).
Every discovered callsite remains in the CSV and section. Spans and tracing
events without a metadata `message` field retain a skipped-template error rather
than being parsed as format strings. The subscriber uses the same eligibility
check before template lookup.
`Callsite::is_log` is `true` for `log` macros and `false` for tracing events/spans;
it is saved in both formats and participates in the `(file, line, is_log, module_path)` key.
For tracing, `file`, `line`, and `module_path` are the canonical values stored in
the static `tracing_core::Metadata` object. Relative filenames stay relative,
exactly as the runtime reports them; missing optional modules remain `None`.
`source` separately resolves the filename against the DWARF compilation directory
for template lookup. This also handles impls defined outside their type's module:
the metadata's module wins over the type's DWARF namespace.

For log, the existing DWARF marker method is unchanged: `file` preserves the full
DWARF source path, and `module_path` comes from namespace metadata excluding
function/type scopes. Function declaration references and retained Rust v0 linkage
names can identify scopes even when an enclosing function DIE was discarded.
Modules are never guessed from filenames or package directories.
`site.template` preserves individual `Ok(Template)` / `Err(LookupError)` results.
Artifact readers initialize the source lookup path as `PathBuf::from(file)`.
`--source-root` relocates relative `file` values, including canonical tracing
filenames; absolute filenames remain intact.

`Callsite::wrapper_row` links a recovered log invocation to its wrapper's declared location.
It is a zero-based index in the returned vector. CSV exposes this index as `row_id`
and stores the foreign key in `wrapper_row`; an empty value means no wrapper.
These are logical rows, not physical CSV line numbers (quoted fields can span lines).
The section uses the same indices. Reordering/removing rows requires remapping links;
writers/readers reject dangling references, cycles, and cross-framework links.

## Extraction and formats

The tracing matcher locates retained `__CALLSITE: DefaultCallsite` objects through
DWARF and follows their `meta` pointer. It decodes the metadata's file, line,
kind, field names, and optional module using DWARF member offsets, string layouts, and enum/niche
variants, with the input ELF's pointer width and byte order. It applies dynamic
relative/absolute pointer relocations for PIEs and shared libraries. No Rust
struct offsets are hardcoded, and the input is never executed. Discarded statics
without addresses are skipped; unsupported retained layouts/relocations or invalid
metadata return an extraction error rather than falling back to inferred values.
This requires full matching DWARF; it does not promise discovery in stripped
binaries or support arbitrary custom `Callsite` implementations. The current
`Callsite` API requires a file and line, so retained metadata missing either
cannot be represented and produces an explicit error. Static metadata does not
contain the source message template or argument expressions.

The log matcher still uses macro-generated `lvl: log::Level` locals and the
existing DWARF namespace/location inference. Log detection depends on that private macro
implementation and on full debug information retaining the local variable.
Source lookup removes ordinary `lvl` locals and `log_enabled!` checks; candidates
whose source is unavailable remain error records.

The scanner then reads the source macro at each location and extracts decoded format
strings, argument expressions, and explicit tracing fields, preserving Rust
syntax with normalized token spacing. Raw strings, multiline invocations,
nested expressions, named arguments, and `message = format_args!(...)` are
supported. Span names are not parsed as templates.
For messages, `args` maps decimal positions or names to Rust expressions:
`"{} {name} {user:?}", value, name = other` produces
`{"0":"value","name":"other","user":"user"}`. Implicit captures and named
width/precision arguments are included; escaped braces do not create arguments.

`fields` maps field names to expressions, preserving formatting modifiers.
For example, `?user, item.id = %id, count` produces
`{"user":"? user","item.id":"% id","count":"count"}`. Log capture modifiers
retain their colon syntax, e.g. `user:?` becomes `{"user":":? user"}`.

Source lookup also traverses `once!`, `cfg_if!`, and `cfg_select!` wrappers.
Balanced token groups stop each multiline call at its matching closing delimiter,
ignoring parentheses in strings/comments. Other macro bodies stay opaque so nested
formatting macros cannot be mistaken for separate logging calls.

Log macros support `target:`, `logger:`, the explicit level in `log!()`, and
structured key/value fields before `;`. Format argument parsing is shared with
tracing; each framework keeps its own field grammar. Single-invocation locations
use the framework identified by metadata even when an import renames its source
namespace (for example, `use bevy_log::{self as log}`). Qualified log and tracing
calls on the same line remain separate; ambiguous macros within one framework
produce errors. `lookup_templates` takes a mutable `Vec<Callsite>` so it can remove
log marker false positives after examining source.

Wrapper recovery uses the marker's DWARF declaration and its enclosing function
or inline-origin declaration. Source lookup locates the enclosing `macro_rules!`
definition, then finds invocations with that name inside those identified function
bodies. For log, both declaration and invocation rows are kept, with the invocation
pointing to the declaration. Tracing already supplies the canonical invocation
location: declaration evidence is used only to recover its template, without
adding a second location or wrapper link. No wrapper-name list or runtime dump is used.

Templates are interpreted only for verified whole-token forwarding patterns such
as `($($args:tt)+) => { log::info!($($args)+) }`, using the usual log/tracing grammar.
Rewritten arguments and invalid format syntax produce error rows. Local macro
shadowing, unresolved qualified names, and conditional invocation attributes are
excluded conservatively. Nested functions/closures need their own DWARF scope.
Source recovery identifies potential compiled-function invocations, not proof that
a branch executed; source files must match the binary.

`#[track_caller]` can make a source log statement report several caller locations.
Declaration locations alone do not establish those aliases. The scanner leaves
caller-dependent locations unresolved unless DWARF/source evidence identifies them;
it does not invent links from a runtime dump or matching message text.

Unsupported/missing templates, unavailable source, and multiple macros on one
line become per-callsite errors with the original source line when available.
Source files must match the build. Extraction never executes the input binary.

CSV columns, in order: `file,line,result,format,args,fields,source,error,is_log,module_path,row_id,wrapper_row,type,macro_call`.
`is_log` is `true` or `false`. Legacy eight-column CSVs load as tracing records;
eight- and nine-column CSVs have no module metadata and load it as `None`.
Legacy eight-, nine-, and ten-column CSVs have no wrapper references.
`args` and `fields` contain JSON dictionaries; `result` is `Ok` or `Err`.
Readers also accept legacy expression arrays and convert their stored entries
to dictionaries; implicit captures absent from old artifacts remain unavailable.
The `source` column holds the source **line text**, including successful and
skipped rows, not a source path. `macro_call` holds the complete original macro
invocation, preserving whitespace, comments, and newlines. If the invocation
cannot be identified (including ambiguous same-line calls), it contains the source
line instead. Both fields are empty if the source file cannot be read. The same
text is retained in `Callsite::source_line` and `Callsite::macro_call`, and in the
section. Quoted commas, quotes, and newlines are supported. Legacy CSVs without
`type` default non-log records to `event`; their span classification was not stored.

`templates.bin` contains the raw section payload, ready for post-link embedding.
`templates.o` matches the input ELF's architecture and byte order and contains
the identical payload in a read-only `.tracing_scan_elf` section, plus boundary
symbols for linking. For example, with GNU objcopy:

```sh
# Embed in place, adding or replacing the section using the nearest target ancestor.
./tracing_scan_elf/embed_section.sh path/to/target/debug/executable

# Override the payload path (also works for executables outside a target directory).
./tracing_scan_elf/embed_section.sh path/to/executable path/to/templates.bin
```

The script requires Bash, `realpath`, and GNU `objcopy`. Set `OBJCOPY` to override
the tool executable. Without a second argument it walks up to the nearest
`target` directory and reads `tracing_scan_elf/templates.bin` there, matching the
scanner's default. It fails if no `target` ancestor exists.
The equivalent explicit command, writing a separate executable, is:

```sh
# Add or replace the non-allocated section; the reader opens the executable file.
objcopy --remove-section=.tracing_scan_elf \
  --add-section .tracing_scan_elf=target/tracing_scan_elf/templates.bin \
  --set-section-flags .tracing_scan_elf=readonly,contents \
  path/to/executable path/to/executable.with-templates
```

The payload is relocation-free. All integers are **little-endian u32**, regardless
of the ELF's byte order:

| Offset | Content |
| --- | --- |
| 0 | Eight magic/version bytes: `TSCAN\0\x06\0` |
| 8 | Record count |
| 12 | Records, 36 bytes each |
| After records | UTF-8 filenames, JSON result payloads, and module names |

Each record contains `(file_offset, file_length, line, result_offset, result_length,
is_log, module_offset, module_length, wrapper_row)`. Zero module length means unavailable metadata.
`wrapper_row = u32::MAX` means no wrapper; other values are zero-based table indices.
The boolean is encoded as a u32, strictly 0 or 1. Readers accept v1 (20-byte)
and v2 (24-byte) records with `module_path = None`; v1 also defaults `is_log` to false.
V3 and v4 use 32-byte records without wrapper references. V3 stores expression arrays;
v4 and v5 store dictionaries. V6 keeps the v5 record layout and adds classification
and source text to the JSON payload. All older section versions remain readable;
legacy non-log records default to `event` because they did not store span kind.
Offsets are section-relative; strings have no terminator. Extraction sorts by
`(file, line, is_log, module_path)`; recovered invocation rows follow the declarations.
Writers and readers preserve that order and its references. V6 JSON payloads
contain `type`, `source_line`, `macro_call`, and `template`. The template is still
`{"Ok":{"format":"...","args":{"0":"value"},"fields":{"user":"? user"}}}` or
`{"Err":{"line":"original source line","error":"reason"}}`.
