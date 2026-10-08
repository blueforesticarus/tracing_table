# Streaming dump compression experiments

Run the example in release mode for timing comparisons. Each comma-separated
CLI dimension expands into a Cartesian product of experiment rows. The dump
is read once, record by record. Each archetype's experiments finish before its
strings are released; global dedup retains only distinct selected strings until
the final experiment. No output is printed per archetype.

Storage reports and written findings default to **bytes per original matched
row**. Use the same row count for all dedup modes, including rows without
selected strings. Keep message payload, dictionary cost, and total storage
separate. Dictionary and sample-size configuration values remain absolute bytes.

This command produces the timing, compression size, and string summary tables
for all four dedup modes, both block sizes, and both field groups:

```sh
cargo run --release --example dump_message_compression -- \
  --dedup=plain,block,archetype,global --block_size=100,1000 \
  --compression=none,lz4,zstd --compression_mode=blocks,whole,dict \
  --fields=message,other --timing --sizes --summary \
  ./doc/tracing_dump_1791408355268947615.msgpack > compression.csv
```

Use only `--fields=message` for the main message comparison. Use `--timing`,
`--sizes`, or `--summary` alone to select one report; all are enabled by default
when no report flag is provided. Reports are labeled CSV sections, each with
a header. Columns always use the same units and dimension order. Both reports
identify each codec with a `compression` column and its layout with
`compression_mode`. Block sizes are adjacent within each dedup mode
when the remaining settings have one value.

| Argument | Default | Meaning |
|---|---|---|
| `--dedup` | `plain,block,archetype,global` | Keep all strings, the first occurrence per block, per archetype, or across all matching archetypes. |
| `--prefix` | `off` | `off,on` compares whole messages with splitting each row block's longest common prefix before deduplication. Enabled only for message fields with block/archetype/global dedup. |
| `--block_size` | `100` | Row indices per block, including rows with no selected strings. Empty blocks are omitted. |
| `--fields` | `message` | `message`, or all non-message string fields together as `other`. |
| `--compression` | `none,lz4,zstd` | Uncompressed storage, LZ4, or Zstd. |
| `--compression_mode` | `blocks,whole,dict` | Ordinary blocks, whole archetypes, or dictionary compression (blocks or individual messages, depending on dedup). `none` applies to the uncompressed case. |
| `--zstd_level` | `3` | Zstd compression level. |
| `--dict_size` | `8192` | Maximum bytes per dictionary pool (archetype or global). |
| `--sample_size` | `256` | Plain/block dedup training samples concatenate whole messages until this target is reached. Archetype/global dedup uses individual messages instead. |
| `--target` | `*` | Target glob; multiple values create independent target selections. |

All dimension arguments accept multiple comma-separated values. For example,
add `--zstd_level=1,3,6 --dict_size=4096,8192 --sample_size=256,512` to compare
those settings. Both `--name=value` and `--name value` work; option names also
accept hyphens in place of underscores. Settings come from the CLI rather than
the old `MESSAGE_BLOCK_ROWS`, `ZSTD_DICT_BYTES`, and `ZSTD_LEVEL` environment variables.

List sorted distinct callsite targets, one per line, and exit without loading
the codecs or running experiments:

```sh
cargo run --release --example dump_message_compression -- \
  --list_targets ./doc/tracing_dump_1791408355268947615.msgpack
```

Export all original message occurrences for further string/prefix exploration:

```sh
cargo run --release --example dump_message_compression -- \
  --dump_strings=messages.tsv ./doc/tracing_dump_1791408355268947615.msgpack
```

The output file has no header, with one line per message:
`<unique archetype id>\t<target>\t<message>\n`. The unique ID is the numeric
`callsite_id:archetype_id` pair, because archetype IDs are local to a callsite.
Duplicate and empty messages are preserved in dump order. Only string message
fields are exported. This mode reads one record at a time, loads no codecs,
and exits after writing the file. It creates a new file and rejects an existing
output path. `--target` filters apply once per message even when patterns overlap;
field, dedup, prefix, compression, and report options do not transform the export.

Target and message columns escape backslashes as `\\`, tabs as `\t`, newlines
as `\n`, and carriage returns as `\r`, so every occurrence occupies one physical
line. Decode those escapes in both columns before analyzing the strings;
all other UTF-8 text is preserved.

Inspect the TSV with a shared prefix per archetype, or per block of exported
messages (the default is the whole archetype):

```sh
python3 examples/dump_messsage_compression/print_strings.py \
  --block_size=100 messages.tsv
```

For templates with variable fields, use the Rust inspector. It runs all seven
methods by default; `--algo=mdl` selects one, and comma-separated names select
several. `seq` is an alias for `sequitur` and its label in the report.
`--block_size` has the same meaning as above. Both inspectors omit
archetypes with only one message and print an empty line between blocks and
archetypes. The Rust inspector reads the original TSV directly; it does not
run the Python prefix splitter first.

```sh
cargo run --release --example print_strings_mdl -- \
  --block_size=100 messages.tsv
```

Add `--colored` to render the template column in bright cyan in a terminal.
Color is off by default, so ordinary output remains plain TSV.

Every completed run overwrites `examples/dump_messsage_compression/formats.result`
with the exact timing and string-size summary bytes printed to stderr, including
headings, alignment, and ANSI colors.
The file is colocated with the example regardless of the working directory.

At completion, stderr reports each selected method's total time in milliseconds
and row-weighted average milliseconds per block. The
average is `sum(block_ms * block_rows) / sum(block_rows)`, so a short final block
has less weight than a full block. Both millisecond columns use decimal notation
with precision chosen to retain three significant figures in small measurements.
Each timing column uses the same number of decimal places, padding larger values.
The report columns are space aligned. Each numeric column
is colored independently from its smallest value (bright blue) to its largest (red); leading zero digits use dark grey, and other equal values
use the midpoint color. Timing includes
clustering and template extraction, including attempts that produce no template. It excludes
TSV reading/tokenization, result merging, and printing. Single-message archetypes
are skipped and do not count; an empty run reports `n/a` for the average.
Each method carries learned templates and a small set of unmatched examples
across blocks within an archetype. Cached matches can promote values that are
constant in the current block into its printed template. The cache resets for
the next archetype. This is a bounded exploration cache, not a persistent
Drain3 tree or full Sequitur grammar.
Use `--no-carry` to reset learning after every block for comparison.

The stderr report also shows unique template bytes, unique value bytes, and
value bytes with duplicates retained, separately for each method. Every byte
column is divided by the number of rows in archetypes with at least two
messages. Singleton messages still contribute whole-string values to the byte
numerators, but are excluded from this divisor. When there are no eligible
rows, byte columns show `n/a`. Byte figures use three significant figures with decimal K/M/G suffixes (powers of
1,000); values below 1,000 bytes have no suffix. Deduplication
spans all processed blocks and archetypes, with separate pools for templates
and individual values. Counts use UTF-8 bytes before TSV escaping or coloring;
templates include their `{}` markers. Results are counted independently for each
method, even when their output row is merged with another method's result.
When a method produces no template, the whole original message is counted as
a value, including messages from singleton archetypes omitted from display.
Those strings share the same deduplication pool as extracted values and add no
template bytes.
These byte figures exclude reference/storage overhead, and size accounting is outside
the algorithm timing.
`total` sums deduplicated template and value bytes. `total_lz4` compresses those
same bytes as one LZ4 block per method, concatenating sorted templates followed
by sorted values. It excludes string boundaries, row references, and framing;
this estimates string-pool compression rather than a complete storage format.
Compression is also outside the algorithm timing.
In the compact size table, `tpl` and `val` are deduplicated bytes per row,
`all_val` retains duplicate value bytes, `total` is `tpl + val`, and `lz4`
is their compressed size per row. `tpls` counts unique templates; `vals`
counts all stored value slots across messages, including whole-message values when extraction
fails. These counts help estimate the row-reference overhead excluded above.

Each Rust output row is `tag<TAB>archetype<TAB>target<TAB>template<TAB>value...`.
The seven-character tag uses `a`, `q`, `m`, `d`, `s`, `p`, and `n` for alignment,
Sequitur, MDL, Drain, Spell, simple prefix, and no algorithm; spaces fill inactive positions. Results for a message
are printed together in input order. Methods with identical templates and
values share a row and combine their tag letters. A method that finds no
useful template for a block or cluster prints no result for it.
Each `{}` in the template consumes one value field in order. Escapes use the
same convention as the TSV export; literal braces in templates are `\{` and
`\}`. These are fast exploratory variants rather
than exact reproductions of the research implementations:

| `--algo` | Method used here |
| --- | --- |
| `msa` | Progressively align message tokens with longest common subsequences, then keep shared anchors. |
| `sequitur` | Replace repeated token pairs with up to 12 grammar rules, group by a shared rule, then extract a template. |
| `mdl` | Choose shared position-aligned tokens that minimize template plus residual byte cost. |
| `drain` (or `drain3`) | Group by token count and first token, then use a positional similarity threshold. |
| `spell` | Maintain online clusters with longest common subsequence templates; use token overlap to select a candidate cluster. |
| `prefix` | Split a shared Unicode-safe prefix from each message in a block. |
| `none` | Store every original message as one whole-string value. |

For speed, `msa` samples 32 messages before validating anchors against the
entire group, `spell` keeps at most eight clusters per leading token, and very
long pairwise alignments are skipped. The first five methods score the final
shared anchors with the same byte-cost model so their printed fields have a
consistent format.

Filter reports to one target group or compare several target groups:

```sh
cargo run --release --example dump_message_compression -- \
  --target='glfw*,corngame' --fields=message,other --summary \
  ./doc/tracing_dump_1791408355268947615.msgpack
```

`glfw*` matches `glfw_core::whatever`. A bare `corngame` matches `corngame`
and `corngame::whatever`, but excludes `corngame_extra`. `*` matches arbitrary
text and `?` matches one character, including across `::` separators. Quote
globs to prevent shell expansion. Overlapping target selections are independent;
their rows should not be added together. `--list_targets` also accepts this filter.

The timing table has one row per setting/codec/layout permutation and four timing
columns: `compress_total_ms`, `decompress_total_ms`, `compress_unit_avg_us`, and
`decompress_unit_avg_us`. `unit` identifies a block, a whole archetype, or an
individual message; `units` gives the count. Both averages use uncompressed-byte
weights: `sum(unit_time * uncompressed_unit_bytes) / sum(uncompressed_unit_bytes)`.
Equivalently, normalize each unit's size by the average unit size and average
the weighted times over the number of units. Ten equal-sized blocks taking
2 microseconds each give 2 microseconds. Totals sum individually timed codec
calls; output allocation is included, while validation, reading, deduplication,
and dictionary training are excluded. Every timed decompression is checked
against its original input. Timings naturally vary between runs.
Zstd reuses its compression/decompression contexts but supplies dictionary bytes
to `usingDict` on each call; preprocessing into reusable `CDict`/`DDict` objects
is not part of this benchmark.

The uncompressed case is always one
`compression=none, compression_mode=none` baseline, independent of the selected
compressed layouts. Its stored size equals its deduplicated raw size and its
ratio is 1. Compression and decompression totals are zero because no codec work
is performed, and unit averages are `n/a`. Dictionary size, sample size, and
Zstd level are `n/a` and produce no repeated baseline rows. Block size is also
`n/a` for plain, archetype, and global dedup, but remains a dimension for block dedup.
Use `--compression=none --sizes` to report only uncompressed baselines; it works
without loading codec libraries. `--compression_mode=none` is also accepted
for an uncompressed-only report.

Supported layouts are:

| Dedup | None | Blocks | Whole archetype | Dictionary |
|---|---|---|---|---|
| `plain` | All strings | Row blocks | Concatenated archetype | Row blocks, one dictionary per archetype |
| `block` | Unique per row block | Row blocks | Concatenated deduplicated blocks | Row blocks, one dictionary per archetype |
| `archetype` | Unique per archetype | `n/a` | Concatenated unique strings | Individual unique messages, one dictionary per archetype |
| `global` | Unique across selection | `n/a` | `n/a` | Individual unique messages, one dictionary across the selection |

Unsupported permutations are printed as `n/a` and skipped. `whole` always means
one compression pass per archetype. Whole-archetype compression is a reference
point only and is not viable for the implementation; supported implementation
choices must allow independent block or message decoding.
With prefix mode off, block size and sample size are irrelevant to
archetype/global dedup; they are `n/a` and do not create repeated permutations.
With prefix mode on, block size affects the split for every dedup scope and
remains a dimension; sample size is still irrelevant to archetype/global dedup.

## Prefix splitting

Use `--prefix=off,on` to compare both variants. Prefix mode requires dedup and
message fields: combinations with plain dedup or other fields are omitted from
the Cartesian product. A command with no allowed combinations reports an error.

Before deduplication, inspect all messages in each original row block and take
their longest common prefix at a UTF-8 character boundary. Store a nonempty
prefix once for the block and split each message into that prefix and its suffix.
If there is no common prefix, retain each whole message as a suffix. A block
with a single message uses that whole message as its prefix and an empty suffix;
identical messages likewise produce empty suffixes.

Prefixes and suffixes use separate interning pools, even when their bytes match.
Both pools follow the selected dedup scope: block, archetype, or global. Prefix
detection always stays within a row block and never spans archetypes. Global
pools are distinct for each block size and field/target selection. Both prefix
and suffix storage contribute to the reported payload; row references and
prefix/suffix association metadata are excluded, as in earlier experiments.

Block dedup concatenates the retained prefix and suffix strings in each block
before ordinary compression. Archetype/global dedup still has no compressed
block string payload. **Prefix + dictionary compression is discouraged.** It
is supported for completeness: training uses both retained prefixes and suffixes,
and archetype/global dictionary compression stores each retained part as
`min(raw, compressed)`. Such timing/size rows identify their unit as `string_part`.
The summary table continues to describe original full strings rather than parts.

Compare prefix splitting before compression and after regular block compression:

```sh
cargo run --release --example dump_message_compression -- \
  --dedup=block --prefix=off,on --block_size=100,1000 \
  --compression=none,lz4,zstd --compression_mode=blocks --sizes --summary \
  ./doc/tracing_dump_1791408355268947615.msgpack
```

To compare raw archetype/global pools, use
`--dedup=archetype,global --prefix=off,on --block_size=100,1000 --compression=none --sizes`.

## Storage and string summary reports

The size table has one row per codec/layout comparison. It reports
`raw_bytes_per_row`, `payload_bytes_per_row`, `stored_bytes_per_row`, and their
ratio, along with unit and dictionary metadata. `rows` is the original matched
row count for the field/target selection, even when only `--sizes` is selected.
All measured byte columns divide exact internal totals by that count, formatted
to six decimals; zero-row selections report `n/a`. Stored bytes per row are
payload plus dictionary storage per row; `dict_bytes_per_row` reports the
dictionary cost separately. Dictionary training uses Zstd for both codecs;
LZ4 uses the resulting bytes as its external dictionary. Pools without enough
training data fall back to ordinary blocks for plain/block dedup, and raw
messages for archetype/global dedup. Counts of trained, untrained, and empty
pools are included. The ratio is deduplicated input bytes divided by stored
bytes. The accompanying `dictionary_training` table
reports the successfully trained subset's bytes amortized over the same full
selection row count, including separate payload and dictionary storage sizes.

For plain/block dedup, training uses all of an archetype's retained strings,
grouped into samples that reach roughly 256 bytes by default. Messages are never
split, so a sample can exceed the target and the final sample can be shorter.
Grouping can span row blocks. All samples are accumulated before training,
then the blocks are compressed using the resulting dictionary.

For archetype/global dedup, each distinct message is one training sample. Once
the complete pool is collected and its dictionary trained, each message is
compressed individually, storing whichever is smaller: raw or compressed.
`raw_units` and `compressed_units` report how many messages use each choice.
Compression timing includes attempts whose results are discarded. Raw messages
make no decoder call: their zero codec time and original bytes are included in
the decompression average. The global pool spans all matching archetypes for
each field/target selection; overlapping selections get independent dictionaries.

To compare the new message dictionary options alone:

```sh
cargo run --release --example dump_message_compression -- \
  --dedup=archetype,global --compression=none,lz4,zstd \
  --compression_mode=dict --timing --sizes \
  ./doc/tracing_dump_1791408355268947615.msgpack
```

Training and evaluation use the same data. Stored references to removed strings,
message lengths, and tags indicating raw versus compressed storage are excluded
from every compression size. These are codec payload experiments rather than
a complete serialized storage format.

The string summary table is independent of compression settings, with one row
per field/target selection. It includes rows, string occurrences, original bytes per row,
unique strings per archetype and across the selection, duplicate bytes within
archetypes, and cross-archetype sharing. Duplicate byte columns also use bytes
per original matched row. Cross-archetype duplicate bytes per row are
`sum(string_length * (number_of_archetypes_using_string - 1)) / rows`. It also lists the
selected string field names. Exact sharing statistics retain distinct strings
across archetypes while records stream past.

`--summary` also runs without loading codecs. If libzstd cannot be located by
the system loader or `pkg-config`, set `ZSTD_LIBRARY` to its shared-library path.
Use `--help` for the CLI reference.

## Individual message dictionary results

The supplied `tracing_dump_1791408355268947615.msgpack` was measured in release
mode with prefix mode off, Zstd level 3, and an 8,192-byte maximum dictionary size. The complete
[CSV results](../../doc/message_dictionary_comparison_2026-10-07.csv) include both
field groups and the existing block/whole comparisons at 100 and 1,000 rows.
That historical CSV retains byte totals from the original run; new command
output defaults to bytes per row.
All measured storage values below are bytes per original dump row: each byte
total from the CSV is divided by **396,287 rows**, including rows without a
message. The same denominator applies to every dedup mode; it is not the number
of unique messages or compression units. Values are rounded to three decimals.
Dictionary limits and CLI sample-size settings remain absolute byte counts.
The message dictionary results are independent of row block size:

| Dedup | Codec | Payload B/row | Dictionary B/row | Stored B/row | Raw / compressed messages |
|---|---|---:|---:|---:|---:|
| Archetype | LZ4 | 6.385 | 0.531 | 6.916 | 650 / 102,147 |
| Archetype | Zstd | 8.184 | 0.531 | 8.715 | 8,940 / 93,857 |
| Global | LZ4 | 8.680 | 0.021 | 8.701 | 3,788 / 98,902 |
| Global | Zstd | 10.155 | 0.021 | 10.176 | 35,174 / 67,516 |

| Dedup | Codec | Compress total (ms) | Decompress total (ms) | Compress weighted mean/message (µs) | Decompress weighted mean/message (µs) |
|---|---|---:|---:|---:|---:|
| Archetype | LZ4 | 133.841 | 5.976 | 7.102446 | 2.682085 |
| Archetype | Zstd | 818.993 | 455.117 | 18.101688 | 7.830991 |
| Global | LZ4 | 141.507 | 6.362 | 7.557596 | 2.704312 |
| Global | Zstd | 821.162 | 324.980 | 19.203198 | 7.567462 |

Global dedup removes only 107 additional message instances and 0.014 raw B/row
(0.074%) beyond archetype dedup. Its smaller dictionary footprint does not
offset its larger payload: total storage is about 26% larger for LZ4 and 17%
larger for Zstd than with archetype dictionaries. Fifty archetypes successfully
train dictionaries, 129 fall back to raw messages, and 16 contain no message
bytes. The global dictionary successfully trains once.

LZ4 produces smaller individual-message payloads than Zstd on this corpus.
Whole-archetype compression after archetype dedup remains substantially smaller:
3.684 B/row for LZ4 and 1.816 B/row for Zstd. These whole-archetype results
are reference points only, not viable implementation options. Individual message compression
allows each stored message to be decoded separately, with a size cost here.
Its measured Zstd times include dictionary setup on each codec call as described
above. All 64 earlier applicable compression size rows and both summary rows
were unchanged by adding the message dictionary options.

### Archetype dictionaries versus regular block compression

This comparison uses archetype dedup with individual-message dictionary
compression, versus block dedup with ordinary compression (no dictionary).
Dictionary storage is included; references and storage metadata are excluded
for both layouts. Archetype dictionary results do not depend on row block size.

| Codec | Rows/block | Regular block B/row | Archetype message payload B/row | Archetype dictionary B/row | Archetype total B/row | Total size change |
|---|---:|---:|---:|---:|---:|---:|
| LZ4 | 100 | 7.345 | 6.385 | 0.531 | 6.916 | −5.8% |
| LZ4 | 1,000 | 4.909 | 6.385 | 0.531 | 6.916 | +40.9% |
| Zstd | 100 | 4.222 | 8.184 | 0.531 | 8.715 | +106.4% |
| Zstd | 1,000 | 2.465 | 8.184 | 0.531 | 8.715 | +253.6% |

Message payload includes compressed messages and messages kept raw when smaller
or when dictionary training is unavailable. Dictionary bytes are summed across
archetypes, then divided by the original row count. Regular block compression
uses no dictionary; its reported storage values
are entirely block payload. Total size change compares archetype message payload
plus dictionaries against regular block payload.

For this dump and these settings, archetype dictionary compression saves storage
only against the 100-row LZ4 baseline (5.8%). Regular 1,000-row blocks are smaller
with either codec. Block dedup with regular Zstd compression at 1,000 rows gives
the smallest storage among these viable options: 2.465 B/row. Archetype
dictionaries would need a separate-message decoding benefit to justify their
larger storage footprint relative to 1,000-row blocks. Whole-archetype compression
remains a reference point only and is not a viable implementation option.

CONCLUSION: compressing individual strings in a global/archetype interning table is not worth it.

## Block prefix results

Measured on the supplied dump using the prefix comparison command above, block
dedup, ordinary LZ4/Zstd compression, and Zstd level 3. The command output is
saved in [prefix comparison CSV](../../doc/prefix_comparison_2026-10-07.csv).
All byte values use the same 396,287 original rows. Before compression means
the retained string payload after block dedup, including both prefix and suffix
storage when prefix mode is on.

| Rows/block | Storage | Prefix off B/row | Prefix on B/row | Saved B/row | Reduction |
|---|---|---:|---:|---:|---:|
| 100 | Before compression | 32.361 | 23.092 | 9.270 | 28.6% |
| 100 | LZ4 blocks | 7.345 | 6.706 | 0.640 | 8.7% |
| 100 | Zstd blocks | 4.222 | 4.034 | 0.189 | 4.5% |
| 1,000 | Before compression | 24.974 | 17.449 | 7.525 | 30.1% |
| 1,000 | LZ4 blocks | 4.909 | 4.340 | 0.568 | 11.6% |
| 1,000 | Zstd blocks | 2.465 | 2.321 | 0.144 | 5.8% |

Prefix splitting removes about 29–30% of the raw block-dedup string payload.
The incremental benefit after compression is smaller because ordinary codecs
already encode repeated prefixes efficiently: about 9–12% for LZ4 and 4–6%
for Zstd here. The smallest measured payload remains the 1,000-row Zstd variant,
now 2.321 B/row with prefix splitting. Prefix/suffix association metadata and
row references are excluded, so these are payload savings. All prefix-off size
and summary results match the baseline from before this feature was added.
