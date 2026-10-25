THIS FILE UNFORTIONATELY MIXES CODEX GENERATED DOCS AND HUMAN DOCS

This crate implements an memory-efficient cache for tracing logs, with support for per-callsite rollover. 

The default storage report now combines the Bevy and Actix Web reference captures.
The [Bevy example](examples/bevy_capture/README.md) is a standalone offscreen
dump generator; run it with `make capture-bevy`.
The [Actix example](examples/actix_capture/README.md) uses tracing middleware and
concurrent mock service work, targeting 100 MiB of MessagePack before LZ4, like Bevy.
Generate it with `make capture-actix`, then run `make storage-charts` to compute
`doc/reference_archived_storage.csv` and its SVG/PNG charts from both dumps.
CSV rows retain `run=bevy` or `run=actix`; chart statistics pool both populations.
See the [storage pipeline](doc/storage_chart.md) for single-run overrides and tuning.

Archived frames use bit compaction by default. Build with `--no-default-features`
to use byte compaction, or set `default-features = false` on the dependency.
Bit frames pack varying bits continuously across row boundaries by default.
Set `CONTINUOUS_BIT_PACKING` in `src/data/dataframe.rs` to `false` and rebuild
to restore byte-aligned packed rows. The constant does not affect byte compaction.
[Measured continuous-packing results](doc/continuous_packing_2026-10-22.md).

Parent references store a u64 ZigZag index delta and a u16 dictionary index.
The parent-table portion of each compacted frame's footer stores
`(CallsiteArchetype, FloatU16 offset, FloatU16 stride)` entries,
8 bytes each. `CallsiteArchetype` packs a 20-bit callsite and a 12-bit archetype
into one u32. All entries retain their identity, including singletons.
Active blocks use a zero stride and an offset from the first matching parent.
At initial archival, each identity's first and last matching rows define a line.
Its intercept is the offset; a negative intercept instead fits a line through
the origin and the last matching point. Flat, decreasing and singleton groups
use a zero stride and the first matching index as their offset.

Stride stores the line's total increase over the actual frame length `L`, so
the predictor at zero-based row `R` is `offset + round(stride * R / L)`.
Both coefficients use the existing unsigned `FloatU16`; residuals are computed
from their decoded values and preserve every parent ID exactly. Fitting and
prediction use integer arithmetic with wide intermediates, including saturated
coefficients and full-range u64 IDs. Parent fitting recomputes only its residual mask.
The frame's existing allocation length and row layout imply the trailer's size.
Both compaction formats preserve these predictors through archived edits without
refitting. Readers return ordinary `SpanId` values.
[Earlier offset-only storage comparison](doc/parent_dictionary_2026-10-22.md).

String indices optionally use prediction at archival, independently for each
ordinary string field and extracted template argument. Period zero uses
`start + row`; periods 1–255 use `start + ((row + phase) % period)`, with a one-byte
period and one-byte initial phase. A selected column stores exact u16 ZigZag
residuals; modular arithmetic preserves every u16 index. Consecutive cycles can
have zero residuals even when a block starts partway through the cycle. Arbitrary
repeating patterns still need residuals. Constant and unprofitable columns keep
their raw indices. Selection charges packed payload, footer and allocation padding.

Selected starts live in the footer after the parent table: packed
`(u16 column byte position, u16 start, u8 period, u8 phase)` entries followed by
a u8 entry count. Noncyclic entries store period/phase `0, 0`. The predictor
suffix costs `6 * count + 1` bytes with no entry padding. Its odd length is
distinguishable from the eight-byte parent entry width after dictionaries are
removed. Archived edits preserve coefficients and re-encode changed indices;
block readers and iterators reconstruct ordinary string indices.
[Earlier noncyclic measurements](doc/string_index_start_row_2026-10-23.md).

Archive-only dictionaries additionally support up to 16, 32, or 64 distinct
values per string column, using the actual count's address width. Values remain
in first-occurrence order and store bit-packed unsigned residuals from their
minimum. The minimum reuses the first-row column slot; physical rows store
`minimum XOR encoded_dictionary_id` so ordinary frame packing remains unchanged. A
dictionary is selected only when `(rows-1)*address_bits + count*value_bits +
overhead`, rounded to the actual allocation layout, beats the current raw or
predicted encoding; cyclic prediction of dictionary IDs can further reduce that
row cost. Each dictionary header stores its own period and phase. Period zero
keeps direct IDs, and a nonzero period selects ZigZag ID residuals against
`((row + phase) % period) - phase`, whose first prediction is zero. There is no
stored additive origin for dictionary IDs.

Dictionary entries follow the parent/predictor prefix, then alignment padding,
one backward halfword pointer per selected field, and the terminal u8 field mask.
Pointers measure distance from that mask byte; their maximum value represents
510 bytes. The farthest pointer locates the preceding predictor count, so both
sections remain independently readable. The first eight string-field/template
argument positions are eligible; wider schemas and dictionaries that exceed the
shared 512-byte window retain their existing encoding. Zero masks cost one byte;
generic frames with no strings or parents retain an empty footer. An edit to a
new dictionary value or to row zero's dictionary ID rebuilds ordinary indices
while retaining parent and offset/cycle coefficients.
[Dictionary layout, measurements and verification](doc/string_dictionary_2026-10-24.md).

Bit-compacted frames store the packed-row stride as `u16`: at most **65,535
varying bits per row** with continuous packing, or **65,528 bits** with
byte-aligned packing. The effective maximum for a fully varying uncompacted row
is therefore **8,191 bytes** in either mode. Wider raw rows can fit when enough
bits remain constant; the raw row-width field still permits up to 65,535 bytes.
An oversized packed stride is rejected before archive allocation.

# Rollover
Every callsite (instance of trace!, info!, span! etc.) get's it's own cache of logs, with it's own rollover. This means that quick spamming logs (such as is common for level=trace) will not push uncommon logs (an important error, for example) off the queue. 

# Components
1. smartstring: inlines strings shorter than the pointer length and interns others on the heap, avoiding duplicates.

TODO: this should actually be tuned to take up the same space as the overhead of the string deduplication. Since, strings will very efficiently be deduplicated in the dataframe compaction.

NOTE: I actually don't need this as the string_cache crate supports inlining.
   
2. dataframe: a row-oriented buffer for byte slices of fixed (runtime-specified) length, which can be "compacted", storing only the subset of bytes which change row to row. The compacted frame can be iterated over direcly using a lending iterator.
   
3. blockbuffer: stores an uncompacted "active" dataframe for writing new logs,  when the active buffer has reached ~1Kib in size, it is compacted and moved to a ring. (TODO: When the ring reaches its max entries, the oldest ring is dropped. ) This reduces the storage cost of things like timestamps and counters, since most of the bytes will be constant, while still supporting the full range of possible values. 

Blockbuffers for a specific archetype are initially allocated with only enough space for one log, this doubles untill the active buffer reaches the configured size, and the buffer begins compacting old data. This is to help in the case where there are many archetypes. See: [[archetype proliferation]].

4. archetypes: Logs are stored according to archetype. Each archetype get's it's own blockbuffer. Every callsite + *set of **provided** fields* gets it's own archetype (note that if `myfield: Option<T>` that `trace!(myfield)` will potentially have two archetypes depending on whether myfield is some or none).

Log data is packed into a byte_slice as (timestamp, end_timestamp (for spans), event/span id, parent id, follows_from id, fields...). If the log has no follows_from id, it is not stored (the log will get a different archetype).

Events and logs store a `u16 timestamp_index` immediately after the shifted timestamp.
One recorder-wide last timestamp/callsite/archetype/count tracks runs: the index
increments on a callsite or archetype change within the same tick, stays unchanged
for consecutive rows of the same archetype, and resets to zero when the tick changes.
For nondecreasing timestamps, sort by `(timestamp, timestamp_index, row index)` to
reconstruct event/log order. Spans do not store or advance this index. The counter
saturates at `u16::MAX`, which marks lost precise ordering until the tick changes.

Timestamps are stored as shifted `u64` nanoseconds since the recorder started.
The default shift is **10**, giving **1,024 ns (1.024 µs) ticks**. Set
`RecordingLayer::timestamp_bit_shift` before recording to choose another resolution.

Active timestamp headers store unsigned offsets from the block's fixed first
timestamp. At archival, the first and last start timestamps fit a stride over
the actual row count `L`: the predicted offset at row `R` is
`round(stride * R / L)`. Collocated metadata stores the stride as `FloatU16` in
shifted ticks, keeping small values such as 50 exact even at shift 20.
Strides above `FloatU16::MAX` (4,396,972,769,280 ticks, about 4,397 seconds at
nanosecond resolution) are disabled with zero; those blocks retain unsigned
offsets. Enabled blocks store exact ZigZag residuals from the decoded predictor.
Only timestamp mask bits are recomputed. Readers use the frame's existing row
count; archived edits retain the fitted stride, and late span closes only extend
the timestamp range. Span ends store duration + 1, with zero reserved for an open span.

Event id's are assigned as per-archetype sequential counters, to maximise how many bytes can be compacted. Archetypes ids themselves are also sequential.

Because the archetype is part of the id, following parent/follows_from refs can also be fast.

## Span Id problem.
Ideally the id of an event would simply be it's index in the blockbuffer.
This would eliminate the need to store it at all, resulting in ~2 bytes savings per row. It would also allow id's to be accessed in O(1) instead of seaching the blockbuffer.

However, the id is based on span creation, where-as the span is stored on *completion*. When the span is created we do not yet know it's archetype, as it's empty fields may still be recorded with span.record. Additionally we do not know it's end timestamp. 

Consider the case of a log which is written to the cache. Minutes later it's span ends. When the log is written, there is no way to know where in the buffer the span will go. The ID must be fixed when it is created. Meanwhile, several thousand of these spans have finished while this slow one was running. 

We *could* insert in the compacted old frame. This would not be so bad if we reserved space for it. But we don't know whether all fields will be present (or their type), so we don't know it's archetype. 

We could make fields nullable and unify the archetypes, however:
1. this would add 1 byte (per 8 fields), though it will normally be compacted.
2. it would require some logic to prevent absent fields from registering as non-constant. If a field is either Some(5) or None, the field is not variable, only it's presence.

However if a field's *type* changes, it will still alter the archetype.

Options:
1: Sort frames before/during/after compacting.
2: Lookup function that assumes sorting, and then scans.

Regardless, we will need metadata on min/max id of each dataframe.

**Solution. ID is generated when span is created. Fields provided later are stored when the span ends, in a separate table. This table has a pointer to the first.**

# Archetype Proliferation
TODO: handle pathological case
TODO: compute and benchmark the cost of many archetypes

# Further optimization
## Bit packing
The compact frames could be compacted at the bit level instead of the byte level. 
This would require alot of shifting and masking, and perfomance would need to be benchmarked.
Real application log data would be needed to see whether bit packing would save space.

## String Split
Tracing logs are meant to be structured. However, many programs and librarys use tracing's support for format macro's instead, for example

`error!("problem with entity {}", entity);` with `entity : usize 2623`

This is passed to the tracing subscriber as "problem with entity 2623". We do not have access to the format string literal or fields.
If there are many of these messages for different entities, it will result in lots of string interning. 

As an optimization for this common case, for each callsite with a message the logger remembers a prefix. When a new log is processed, the prefix is used to split the string into (prefix, remainder), possibly shortening the prefix if needed. If the prefix does not overlap at all, then it is discarded and the current log becomes the new prefix. Both the prefix and the remainder are interned, and stored as seperate fields in the dataframe. Because the prefix is expected to be constant, it will normally be compacted. If no prefix is used, the the message is stored in the remainder, and the prefix field is disabled.

Note that prefix shortening *could* point to the same data as the already stored message. However this complicates reference tracking.

Because prefixes are expected to occur per callsite, we could actually check more than the last message.

## Static string detection
`info!("starting server");`

In this log, the message field will be a static string. 
If we can detect this at runtime (by checking if &'static str is pointing to something in rodata) then we can skip the hashmap lookup and all the work of interning it. 

Unfortionately the bounds of rodata are not accessible from code. It may be recoverable from the applications binary, but whether this could be done reliably enough for an safe api exposing `rodata: &'static [u8]` is questionable.

## Collection Thread


# Querying
TODO:
- random access
- time slice / bounded lookup
    - similar to the midi held notes issue
- filtering (by entity id)

# Rust reference viewer

The GTK/xdot viewer accepts cargo-modules `.dot` graphs and rust-analyzer binary
`.scip` indexes. SCIP attributes reference arrows to the enclosing indexed item,
including methods, fields, and associated items. Inherent methods are grouped under a
uniquely resolved type. Trait implementation references point to the trait's
method declaration, while the implementing type has a reference to the trait.

Generate and open either input in the development shell:

```sh
cargo modules dependencies --no-externs --no-sysroot --layout=dot > modules.dot
python3 util/cargo_modules_view2.py modules.dot

rust-analyzer scip . --output index.scip
python3 util/cargo_modules_view2.py index.scip
```

By default, only the input's local package is included: SCIP uses its recorded
project root, while DOT uses the nearest Cargo manifest above the input file.
Without a manifest, an input containing just one crate selects that crate.
Virtual workspaces and ambiguous standalone inputs require `--crate`.

Repeat `--crate` to include multiple crates by name, package directory, or
`Cargo.toml` path. Hyphens and underscores in names are interchangeable; package
paths select the package's indexed library and binary targets. Explicit paths
are relative to the launch directory. Both endpoints of every drawn reference
must belong to selected crates.

```sh
python3 util/cargo_modules_view2.py index.scip --crate tracing_table --crate tracing-core
python3 util/cargo_modules_view2.py modules.dot --crate . --crate ../another-package/Cargo.toml
```

Referenced symbols from selected dependencies can appear even when their
definitions were not indexed. Local variables and parameters are omitted.
Modules named `test` or `tests`, including their descendants and references,
are excluded from both input formats using graph/index data only.
Struct fields are folded into their owning struct: incoming and outgoing field
references attach to the struct, and fields do not appear as separate items.
Enum variants and their fields similarly fold into their enum. Trait impls do
not appear beneath types in SCIP graphs; dependencies from an implementation
body attach to its implementing type. Trait identities are resolved from the
index only; implementations with missing or ambiguous trait identities are
omitted rather than assigned to a guessed trait.
SCIP loading uses only the dump and Cargo manifests; it never reads or scans the
Rust source tree. Module and crate occurrences produce no direct arrows in either
direction, so intermediate namespace qualifiers in paths like
`crate::subscriber::Atom` are ignored. Containers still show proxy arrows for
references to or from their contained items.
Type qualifiers in indexed `MyEnum::MyVariant` and `MyStruct::foo` paths are
also ignored before folding items. This uses indexed ownership and token ranges
separated by two columns (`::`); spaced or multiline qualifiers cannot be
recognized reliably without source syntax and retain their type references.
Occurrences explicitly marked as imports are also ignored.
This preliminary importer uses indexed ranges and symbols; it does not reparse
Rust, merge dumps, or infer calls from trait implementation relationships. Old
or moved indexes may require explicit crate names or a fresh dump.

The importer uses only Python's standard library (Python 3.11+); the viewer still
needs GTK 3, PyGObject, Pycairo, xdot, and Graphviz. Existing appearance variables
and the Style menu also apply to SCIP graphs.

Shift-click a graph item, ownership box, or outline label to limit the view to
that item and its ownership descendants. References crossing that boundary are
excluded. A normal click restores the full reference view; the Style menu's
“Descendants only” option also toggles the mode for the current selection.

Selecting an item shows that item, its direct children, and one outgoing
dependency hop from them, including references summarized by proxies. Disable
“Outgoing visibility only” in the Style menu to include incoming neighbors too.
Newly reached neighbors do not expand further. Self references are excluded.
Neighbors deeper than the deepest selected child are represented by their owner
at that ownership depth; a childless selection uses its own depth. Clicking a
neighbor reveals its own one-hop view. References between displayed items remain
visible even when they did not introduce a neighbor.

Either endpoint of a reference can summarize hidden descendants. Each original
reference uses its nearest displayed source and destination; revealing
a more specific endpoint replaces its parent summary rather than duplicating it.
Proxy arrows have dashed shafts and a source circle when that end carries hidden
references. Fully exact source circles remain omitted. Opposite proxy arrows
between the same displayed items merge into one line with arrowheads at both ends.
Every endpoint marker, on solid or dashed lines, is filled when all represented
references are exact there, hollow when all are proxied, and half-filled when
some are exact and some are proxied. Half fills are drawn in the viewer without
changing Graphviz geometry. Tooltips list underlying proxy references.

If a real reference exists between those items, its solid line and direction take
precedence over proxy lines. Its own exact reference participates in endpoint
classification along with the proxies, so affected ends become half-filled
without an additional dashed line.
Tune `EDGE_MARKER_SIZE`, `EDGE_STROKE_WIDTH`, and `PROXY_EDGE_PENWIDTH` in the viewer.
`MIXED_FILL_SIDE` in `util/cargo_modules_markers.py` chooses the filled half.

Initial placement and routing use ordinary Graphviz, including ownership boxes.
`CLUSTER_MARGIN` controls box padding. “Drag items” is off by default and only
initializes manual geometry after actual movement. Manual positions apply to the
current drawing and are cleared when a selection or appearance change rerenders
the graph. Escape cancels the current drag; “Reset positions” restores the original
Graphviz geometry without rerouting. Panning and zooming remain available.

Run headless graph and importer checks with:

```sh
python3 -m unittest discover -s util
```


# OVERHAUL

1. [x] implement our own string cache
   this needs to handle singleton and repeated strings more intelligently 
   it needs to not inline short strings if it prevents block compaction
   it likely should store seperate index tables for different lengths of string
   it possibly should consider how its design can facilitate prefix and mask representations
   the storage of strings itself should likely be blocks which can be compressed.
   must support deletion
2. [x] support deletion of blocks, which must also cleanup strings
3. [x] order of logs (in a thread, at a minimum, though possibly nominally) must be recoverable. timestamps are not enough
   what we should do, is use timestamp + disambiguator
   disambiguator SHOULD be cross thread so that cross thread timing can be reconstructed, since that timing may be nonarbitrary in the case of locks.
   we can reduce bits in order to 
4. [ ] we might want to allow blocks to grow past default length, untill adding the row would require invalidate a currently compactible bytecolumn
5. [x] support bitpacked version of frame
6. [ ] support compressing frames.
7. [ ] should allow per thread dumping to a collector thread
8. [ ] probably should have bounded lookups for spans active at a given time, which effects block design, might be able to get away without it due to events pointing to span id's. we already have bounded lookup for events/span start/end
9.  we need indexing over fields, for efficient entity lookup
10. [ ] all sources of memory use must be tracked. 
    - mostly done, but malloc overhead unaccounted for
11. [x] we should probably compute the prefix per block, when compacting.
   though this may result in "finished frame 1000|1" "finished frame 1000|2"
   when what we want is "finished frame |10001"
   however whole archetype prefixing means one weird message disables prefixing entirely. 
   *compromise might be reseting when prefix is 0 (or a small number of bytes, 8 feels right due to overhead reasons)*
   but we still should resplit the block on compression
   - **NO LONGER APPLICABLE** prefix feature replaced with template extraction
12. [ ] traitify dyntuple values. support multiple rep formats for a given output type. try to avoid depending on subscriber for soundness of dyntuple actions. may consider nestable / composable dyntuples.
13. [x] need a test which compares just recording every log directly to a file with logs reconstructed from the tracing_table, for accuracy.
14. [ ] we may want to store the blocks columnar, either by field or by byte. This might assist with compression. supporting this is tricky, perhaps should be a compiletime abstraction for perf
15. [x] messages are generally similar per callsite, so it is likely they should be stored together per callsite, possibly in a compressed block. fields are more likely to be shared, so perhaps handle those seperately. compression could use lz4 with a dictionary shared by all the blocks of a given archetype.
   - DONE: what we ended up doing is a little different, but similar in intent
16. [x] however string fields are stored it needs to be easy to equality check, so we can scan for a value like entity="201v1", *this is different than message which has different tradeoffs*
   - DONE: we keep hash tables only for the active string buffers (ie for fast insertion), so this doesn't help with scanning. 
   - scanning for small strings is already fast, 201v1 is 5 bytes
     large strings largely skip by length
   - we ended up not inlining small strings because mixed short and non-short strings would result in very poor compaction. 
   - [ ] **TODO:** create test workload where string indirection is worst case (lots of very short, unique strings, with common prefix) and test. 

## TODO benchmark
64K is a good default lz4 size. my guess is that's as large as we want to go considering random access (following span heirarchy, indexing by entity, etc.)

however, this increases the likelihood of needing more columns in the compacted frame

## NOTES ON PATTERN EXTRACTION
the message is different from other strings fields. handle it special case.

priorities:
1. storage
   - deduplication of patterns
   - compression for large data payload and pathological logs
2. compression speed
   - the case where we care about storage is the case where there are 1000s of logs per second.
   - additionally, we want our system to be general purpose drop in. GOAL: keep within the ballpark of lz4 (which we may use on top) 
     - format extraction must provided a benefit vs raw lz4 which justifies the additional compute. 
3. deletion/free speed
   - when freeing a block, we free its templates and values (assuming no global string cache for now)
4. decompression speed and lookups 
   - we do want to be able to fetch logs for display 

We DO NOT CARE ABOUT
- modification (log messages are immutable)

## Baseline
compressing raw messages with lz4 yields 6 bytes per row
best case with drain yields

## MVP
1. archetype on format of message (takes care of well behaved)
2. store templates once, globally deduped, with cap. 
3. implement our own string interner, which supports multiple instances and custom size
4. lz4 old blocks message values. 

hiccup: if we do message template extraction, then we change archetype when we compact, headache

then:
- static storage for module paths
- evaluate theoretically possible compression by looking at actual logs

## CAPS

20M global cap
256k per callsite cap

for global cap we drop the oldest block for one of the archetypes.
the block which is selected is the one with the largest CompressedSize/TimestampRange
ie. larger blocks and more frequent blocks are given less retention priority.
# Capture/replay benchmarking

The [storage pipeline](doc/storage_chart.md) describes the Bevy and Actix test
dumps, MessagePack/LZ4 capture, deterministic replay, and storage charts.

Set `RecordingLayer::string_compression_level` before recording: `Some(3)` is
the default (HC3), `Some(1..=12)` selects native LZ4 HC, and `None` selects
`lz4_flex`. The environment variable `STRING_COMPRESSION_LEVEL=9` selects HC9;
`flex` selects `lz4_flex`.
Readers support both encoders without configuration.

`RecordingLayer::message_block_min_rows` defaults to `0` (no optional row floor),
or the value of `MESSAGE_BLOCK_MIN_ROWS` in the environment. The byte target
still applies, with capacity for at least one row.

The earlier [string LZ4 level comparison](doc/investigation2_2026-10-15.txt)
reports fast/HC3/HC9/HC12 storage sizes, with
[per-archetype results](doc/investigation2_2026-10-15.csv). It does not measure
compression time.






---







# DEVLOG

EAS: everything below this line is a devlog and was written by me, or least manually copied. The stuff above is an unfortionate mix of my notes and stuff codex produced.

I have attempted to document my attempts to shave down the size, rational and the effect each change had. As well as things I tried which did not turn out beneficial.

Some of the changes I made, some were refactors I had codex make. Some of the later ones I got lazy and let codex rewrite more code than I'd like without reviewing. 

I try to mark LLM produced stuff for my own sanity, but I don't promise it is accurate. I'd like to go over the whole thing with a fine tooth comb and knock out all the things that don't feel like I wrote it, but until then this is a mix of deliberate engineering and throwaway slop. The library is mostly mine, except for dump/replay. The examples and benchmarks and most tests are generated and I haven't even looked at the former two. 

The design of the log storage, for what is worth, was entirely explicit. 

The first version of this library was produced by hand several years ago. Codex has been used to help refactor, clean up, optimize, test, and benchmark it. 

## thoughts on string indirection
EAS:

Any string (or other variable size field) storage requires indirection to be included in the table. 

currently indirection is 4b index per row, with the buffer in block metadata
blocks are 4kib by default, if that was ALL string refs, we'd have 128 string indices, so conservatively we are only storing ~8bits per row per string. 

now, if the string is constant, it will get compacted. so we only care about the case where if varies.

strings have 4bytes in the index + 2 bytes length prefix in the buffer and are probably at least 4 bytes themselves. 10 bytes per string. In the case of all unique strings, message block indexes are 10%. 

If the string varies and there are 128 string indices thats **128bytes vs 20bytes** minimum in the string buffer (2 unique strings). 
- However this is worst case, more likely we'd only need 1 bit to distinguish 2 unique strings, or a few more if other fields also have strings. 
- The worst case is if one field has unique strings per row and the other has 2 unique values at very different indices. resulting in most bits being different.

codex testing

> extraction increase combined (index+string) size for 18 callsites -- 39 decrease, and 29 stay the same within ±5%.
> 24 callsites have constant message and string storage becomes zero with extraction
> 
> this table generated with bitcompact and string indices. only archived blocks considered, active string buffers used by archived blocks were pruned to only strings used in those archived blocks then lz4 compressed before having space counted.

| Extreme | Callsite / template / string fields | Extraction | Unit (indices) | Index | Storage | Combined |
|---|---|---|---:|---:|---:|---:|
| Largest increase, B/row | **287** | Off | 28 (4) | 0.560 | 0.760 | 1.321 |
| | `multi draw indexed indirect count: {:?} {}, ({:?} {})x, max {}x` | On | 44 (20) | 2.262 | 0.325 | 2.587 |
| | | Change | | | | **+1.266 (+95.9%)** |
| Largest increase, % | **240** | Off | 28 (4) | 0.218 | 0.049 | 0.267 |
| | `set index buffer{}: {:?} (offset = {}, size = {})` | On | 40 (16) | 0.755 | 0.031 | 0.786 |
| | | Change | | | | **+0.519 (+194.8%)** |
| Largest decrease, B/row | **177** | Off | 28 (4) | 0.639 | 27.195 | 27.834 |
| Largest decrease, % | `initial overloads for {fun:?}, {:#?}` | On | 32 (8) | 1.588 | 2.039 | 3.627 |
| | | Change | | | | **−24.207 (−87.0%)** |
| Highest index, extraction off | **116** | Off | 52 (12) | 2.060 | 2.449 | 4.509 |
| | `transform components` | On | 48 (8) | 1.718 | 2.445 | 4.163 |
| | , rotation, scale | Change | | | | **−0.345 (−7.7%)** |
| Highest index, extraction on | **239** | Off | 28 (4) | 1.032 | 3.059 | 4.090 |
| | `set vertex buffer {}: {:?} (offset = {}, size = {})` | On | 40 (16) | 3.681 | 1.084 | 4.765 |
| | | Change | | | | **+0.674 (+16.5%)** |
| Lowest nonconstant index, extraction off | **288** | Off | 28 (4) | 0.179 | 0.001729 | 0.181 |
| Lowest nonconstant index, extraction on | `ComputePass::dispatch {groups:?}` | On | 28 (4) | 0.179 | 0.000916 | 0.180 |
| | | Change | | | | **−0.000814 (−0.5%)** |

![Histogram of extraction's change in combined index and compressed-string bytes per archived row](doc/bevy_reference_index_space_histogram.png)

> Includes all 110 message-bearing callsites with compiled templates and archived blocks, without a 5% cutoff: 89 decrease and 21 increase. Negative values mean savings. [SVG](doc/bevy_reference_index_space_histogram.svg) ·
[Per-callsite data](doc/bevy_reference_index_space_histogram.csv).

selective extraction (only were it helps, according to above)
| Extraction | Bytes/row | Total bytes | Bytes saved vs off | Saving |
|---|---:|---:|---:|---:|
| Off | 9.876 | 5,706,608 | — | — |
| Everywhere | 8.781 | 5,079,243 | 627,365 | 10.99% |
| Only the 89 improving callsites | 8.727 | 5,045,954 | 660,654 | 11.58% |

> These payload totals exclude object metadata and unused capacity.
> [Calculation and storage scopes](doc/bevy_reference_selective_extraction.md).

conclusion: *extract everywhere is simpler, and barely worse*

---

- [x] TODO switch to 16bit index to fit up to 2x rows in our 4k block
- TODO make block table metadata a smallvec to allow multiple buffers per block 
       this is better than compressing an index per row because it is monotonically increasing meaning we only need to store the row at which it changes (one u16) not one bit per row (4k bits)
     - XXX: poses issues if span closes and changes strings. strings have to either be immutable or we have to allow expanding archived buffers on change.
- TODO store index as offset, since multiple blocks share string chunks

## timestamps
Wed Oct 21 09:29:36 PM EDT 2026

currently 74% of span data and 40% of event data. around 3.6 Bytes per row per timestamp.

1. IMPROVEMENT: store end timestamp as offset. for nonoverlapping spans it will reduce bits needed. for overlapping spans it could go either way.
- span end timestamp goes from 3.6 to 2.2 B/row 
- overall storage savings of **7.3%** 

2. ADJUSTMENT: default ts shift of 6 (64ns) instead of 3 (8ns) which reduces storage by 6.4%, ts bytes per row is now 3.25 B/row  and end ts offset becomes 1.8 B/row

3. IDEA: stride-offset storage
in any monotonically increasing array of integers, where the range of values is X(X1->X2). The maximum difference between an element Ai's linear value (i/N * X + X1) and it's value is X. This means that the range of values stored as offsets from linear stride is at most the same range as the values themselves. 

The metadata cost is two integers (the initial and the stride, or the two ends).

codex test implementation gives 3% overall reduction. defer for now

4. COMPARE: ts stored as offset from block metadata
   if ts crosses a 2^N boundary it needs N bits, even if range of values is much smaller. 
   codex suggests this will save 2.34% overall, almost as much as stride. 

   > Some blocks shed 12 varying bits per row by removing those boundary-crossing carry bits. Byte rounding absorbs much of the potential saving.

5. parent SpanId
   - is ~1B/row for the index
   - *surprisingly* ~0.3B/row for the callsite+arch, 0.15 of which is varying bits. I would have expect this was virtual always constant.
   - [Parent spanid index experiment](#parent-spanid-index-experiment)

---

![Bevy and Actix archived storage charts](doc/reference_archived_storage.png)

[SVG](doc/reference_archived_storage.svg) · [Storage breakdown and methodology](doc/storage_chart.md)
   
# lz4 sanity check

Updated comparison with the current pie settings: [bit/byte compaction, with and without LZ4 HC3](doc/frame_compression_2026-10-22.md).
Bit + HC3 saves 344,213 bytes (4.18% of the current archive); continuous bit packing
would save 506,197 bytes (6.15%, size model only). The pie now shows row padding
separately. The historical measurements below use an older baseline.

| Form | Bytes | MiB |
|---|---:|---:|
| Uncompacted baseline | 50,947,723 | 48.588 |
| Compacted | 7,853,079 | 7.489 |
| Uncompacted + LZ4 | 15,079,583 | 14.381 |
| Compacted + LZ4 | 7,233,403 | 6.898 |

using HC12 helps lz4 uncompacted (bringing it to 12MiB) but has virtually no effect on compacted+lz4. (it is also much slower ~80ms vs 7.2)

conclusion: compaction is better than lz4, and lz4 on top of compaction saves 7.9% 

TODO: support lz4 compressing frames that especially benefit. 
- you have to compress most frames to get most benefit, it is not dominated by a subset of frames.

# parent spanid index experiment
CODEX:
Parent-index experiment: subtract the first index for each parent
callsite/archetype within a block, or predict it with a fitted first value and
stride. Metadata is built at compaction and retained in exact-length Box slices.
Offset entries are 8 bytes; stride entries are 12 bytes: callsite 2 + archetype 1 +
index 5 + f32 stride 4, with alignment one and no padding. Both use ZigZag
residuals; callsite/archetype remain in each row. Extraction stays enabled.

| Predictor | Payload bytes before | Payload bytes saved | Added Box metadata bytes | Total storage change |
|---|---:|---:|---:|---:|
| First-index offset | 9,476,806 | 207,621 | 437,504 | +220,688 (+2.05%) |
| Stride residual | 9,476,806 | 588,420 | 514,588 | −79,796 (−0.74%) |

Payload includes archived compacted rows and compressed strings, before container metadata.

Stride stores only a base projected to block row zero and an f32 slope, fitted
from each group's first/last actual indices and row positions. No endpoint
positions are retained. Metadata costs include 277,696 bytes of Box handles in
occupied/spare descriptors plus exactly 236,892 bytes for 19,741 stride entries;
there is no spare heap capacity. Total changes also include frame alignment.

Of 12,100 parent-bearing blocks, **12,033 support stride and 67 fall back** to
their original encoding because a projected base is negative and cannot fit the
unsigned 5-byte field. **Zero blocks** exceed the callsite, archetype or original
index widths; zero exceed the positive base range or finite f32 stride range.
All 12,100 support the offset-only encoding. The 376 blocks without parents
allocate no entry arrays and count as zero in the histogram.
[Full investigation, layouts and reproduction](doc/bevy_reference_parent_index.md).

![Histogram of distinct parent callsite and archetype pairs per archived block, counting blocks without a parent as zero](doc/bevy_reference_parent_groups_histogram.png)

[SVG](doc/bevy_reference_parent_groups_histogram.svg) · [Histogram data](doc/bevy_reference_parent_groups_histogram.csv).

---

Experiment: offset/stride

The initial SmallVec metadata increased storage (+5.19% for offset, +6.27% for
stride). Packing entries into exact-sized Boxes made stride slightly profitable.
Further estimates use the measured block sizes to select the cheapest encoding
per block. The single-allocation model stores packed metadata (8 bytes per offset
entry, 12 per stride entry) alongside rows in the existing frame Box, with a
2-byte count/mode field, eliminating separate Box handles.

| Approach | Net bytes saved | Overall saving |
|---|---:|---:|
| Current packed Boxes | 79,796 | 0.74% |
| Selective stride, current Box layout | 214,220 | 1.99% |
| One allocation + selective stride | 422,176 | 3.92% |
| One allocation + cheapest raw/offset/stride | **442,752** | **4.11%** |

All use the same 64 ns baseline. Rows after the first are modeled estimates,
including metadata and 16-byte allocation alignment; they are not implemented or timed.

# reconstructing order / disambiguation of timestamps

- timestamps can be nonunique, losing ordering information
- events are implicitly ordered in archetype
- store a monotonically increasing disambiguator when subsequent events have the same ts but different arch. reset to 0 when ts changes
- only bother for events. allow spans to lose ordering info
- store as u16, if exhausted store u16::MAX and allow losing order info

surprisingly with 64ns ticks we get zero nonunique ts in our bevy dump, and compacted storage is neglibible.

investigating clock res/accuracy:
- back to back calls to Instant::now() give ~20ns
- shift 12 (4us) has no overlaps, shift 14 (16us) 25% of rows share a ts 
  - overall storage drops 14.5% (compared to shift 6), or 3.69% (compared to shift 12) while index grows to 1.65% of event/log payload
  - shift 10 (1us )is good middle of the road. 7.9% improvement vs shift 6 **making this the new default**

TODO: since overlaps are so rare and cheap to store, adding them to **spans** would not be a problem. but for now I am not going to bother. *defering until more of the code takes shape.*

# optimizing block sizes
- switched to min row count of 256 (most rows will be 256)
- for really small rows still targets 4kib

this has brought the overall B/row down from 8.7 to 7.6
- spans 6.7 -> 6.2
- events/logs 8.6 -> 8.2

appears to mostly be in overhead (first+mask)

COST: active went from 2.2M to 5.5M, undoing savings (but better as we collect more logs)
TODO: make active smaller than compact. we can build up compact frames from multiple active frames.
TODO: choose when to compact based on when we lose a constant bit.

*for now default min row count is 0 (disabled)*

# lz4 level
Thu Oct 22 02:00:32 AM EDT 2026

All string buffers. lz4 level comparison.

Same 503 buffers, 7,409,798 raw bytes. Times are medians of seven release runs, measured separately. Times are total for all buffers.

| Level | Total bytes | Compress (ms) | Decompress (ms) |
|---|---:|---:|---:|
| `lz4_flex` | 1,965,193 | 7.367 | 2.844 |
| HC1 | 1,708,153 | 11.251 | 2.788 |
| HC2 | 1,708,153 | 11.366 | 2.608 |
| HC3 | 1,482,898 | 31.541 | 2.659 |
| HC4 | 1,451,153 | 38.138 | 2.518 |
| HC5 | 1,431,356 | 46.452 | 2.591 |
| HC6 | 1,420,958 | 58.449 | 2.586 |
| HC7 | 1,415,061 | 69.647 | 2.626 |
| HC8 | 1,412,712 | 81.495 | 2.607 |
| HC9 | 1,411,602 | 95.079 | 2.609 |

conclusion: HC3 as default.

- string event/log payload: 3.1B/row -> 2.3 B/row
- Archived string storage dropped 24.8%, from 1,884,468 to 1,416,776 bytes. Library and replay checks pass.
- overall storage drops from 8.6 -> 8.35 B/row (3% improvement)

# switching to u16 string indexes
we forcibly rotate string buffer and message block if string buffer gets to 2^16 entries.

Archived storage is now 9,465,582 bytes, down 2.71%. No entry-limits were hit. 
The size savings is mostly just fitting more rows in the block. The active data grew considerably due to more strings left in the active buffer.

# infrequent span metadata
extra span fields and follows_from are both very uncommon (follows_from does not exist in our sample data.)

1. dont store spanfieldsref per row. store it as a `Option<Box<Vec<_>>>` on the block. since vast majority of rows don't need it. simply ensure block metadata has spanfieldsref of every row that has one (deduped of course) this is enough to know whether to scan the extra span fields block when looking up a span. 

2. follows_from is now treated similar to an extra field, the extra fields block is archetyped on its presense

| Measure | Before | Now | Reduction |
|---|---:|---:|---:|
| Total archived storage, including metadata | 9,465,582 B | 8,802,389 B | **7.01%** |
| Span payload | 3,864,239 B | 3,412,127 B | **11.70%** |
| Payload per archived span | 6.616 B | 5.883 B | **11.07%** |
| Container metadata | 1,267,790 B | 1,056,709 B | **16.65%** |

> container metadata went down because: smaller span rows let more rows fit into each 4 KiB block, so we need fewer blocks—and fewer containers around them.

# timestamps are offsets now

store timestamp as offset from block metadata. 
shaves some data by potentially avoiding power-of-2 crossing. -> more constant bits

| Storage | Before | After | Change |
|---|---:|---:|---:|
| Overall | 8,277,942 B | 8,086,422 B | −2.31% |
| Span payload | 3,409,090 B | 3,302,500 B | −3.13% |
| Event/log payload | 4,292,862 B | 4,208,457 B | −1.97% |
| Overhead | 575,990 B | 575,465 B | −0.09% |

TODO: stride

# overhead reductions
- changed compact / bitcompact frame header (unit, length) to u16 which saves 12 bytes per block.
- reduce alignment to 2

*this means blocks cannot be more than 2^16 rows which is fine.*

| Storage | Before | After |
|---|---:|---:|
| Frame headers/alignment | 298,443 B | 57,405 B |
| Archive total | 8,600,133 B | 8,359,095 B |

---

descriptors are still 40B, data overhead 54B
- spare slots (in block vecs) are 174K
- we could move descriptor data to the heap, but this might fuck up alignment and slow indexing
- we could remove the end ts, and use next block start ts. but this means no bounded span info

- We move some data to the blocks heap allocation (for padding reasons and to save on spare vec slots.)
- we define a SmallBox which takes up 8 bytes keeping size on the heap allocation as a u32. This saves 4 bytes nominally and another 8 bytes when null.

> Descriptor: 24 B, down from 40 B. Keeps the row index and timestamp range.
> Deque entries shrink from 56 B to 32 B.
> CollocatedMetadata: 12 bytes moved from Descriptor
> 4 bytes saved by SmallBox for frame, 8 bytes saved on null SpanFieldsRef
> Heap padding increases by 12,554 B

| Storage | Before | After | Reduction |
|---|---:|---:|---:|
| Archive metadata | 814,936 B | 680,722 B | 16.47% |
| Archive including payload | 8,360,799 B | 8,226,585 B | 1.61% |
| Measured resident containers | 12,609,931 B | 12,475,717 B | 1.06% |

*this might be overoptimizing. we are talking on the order of ~50kb of data.*
*it is possible I optimized the storage to the point the (small) overhead is now more relevant to the benchmark data than it would be in-situ*


# Thu Oct 22 01:28:17 PM EDT 2026
I am deleting the original unfinished smartstring file

preserving this interesting comment

```
/// Either an Arc<T> or an inline string of up to 7 bytes.
/// uses the least signifigant bit as the discriminator
/// works because Arc is a pointer that must be aligned
///
/// unfortionately there is good reason not to use a custom allocator
/// https://github.com/rust-lang/rfcs/issues/3540
///
/// although we could setup a custom global allocator, because that could be Size 0
```

# Row Padding
we pad bitcompacted rows to a whole byte
- row padding is 6.2% of the archive

removing padding, continuously bit packing, likely means we cannot do lz4 compression.

| Encoding | Block payload | vs. bit-compacted |
|---|---:|---:|
| Bit-compacted | 6,322,773 B | baseline |
| Byte-compacted | 7,947,824 B | +25.70% |
| Bit-compacted + HC3 | 5,978,560 B | −5.44% |
| Byte-compacted + HC3 | 7,082,762 B | +12.02% |

removing padding is slightly better.

- archive shrank 6.0%, from 8,226,585 to 7,732,597 bytes
- new max row width is 2^16 bits or 8192 bytes since we need to store row width in bits in a u16

# span offset

Storing parent spanid as offset from first does not help much because the blocks were it would help most are the ones with multiple arch/callsite parent ids and hence unrelated indices.

We add a variable length table after frame payload. which stores (callsite, arch, offset)
- collocated metadata stores the count of this table
- payload parent span id callsite/arch stored as index in this table
- payload parent span id index stored as zigzag offset from said entry's offset 
  - *this was is an added benefit -- helps justify the extra metadata*
- offset stored as custom 16bit floating point integer (5 exp, 11 mantisa)

> The uncompressed row width shrank by 6 bytes per parent reference:
> 
> Before: index u64 + callsite u32 + archetype u32 = 16 B
> Now: ZigZag residual u64 + table index u16 = 10 B

For default settings—shift 10, minimum rows 0, extraction on:
| Metric | Before offsets | After | Change |
|---|---:|---:|---:|
| Span payload, B/span | 5.268 | 5.036 | −4.41% |
| Event/log payload, B/row | 6.889 | 6.665 | −3.25% |
| End buffers | 0 B | 112,690 B | New |
| Other container overhead | 680,339 B | 613,239 B | −9.86% |
| Overall archive | 7,720,005 B | 7,376,010 B | **−4.46%** |

parent spanid payload breakdown:
| Component | Span B/span: previous → current | Change | Event/log B/row: previous → current | Change |
|---|---:|---:|---:|---:|
| Parent index → residual | 1.074 → 0.888 | −17.3% | 0.886 → 0.739 | −16.5% |
| Parent identity → table index | 0.266 → 0.095 | −64.2% | 0.226 → 0.057 | −74.8% |
| End buffer: parent table | 0 → 0.115 | +0.115 | 0 → 0.079 | +0.079 |
| **Combined parent cost** | **1.340 → 1.099** | **−18.0%** | **1.112 → 0.876** | **−21.2%** |

---

reconsider floating point offset 
   - offset can be really big while variance is really small, fixed precision might be better.
   - testing with exact u64 offset (6 extra metadata bytes): payload saves only 4K while end buffers grow 60K.
     - *TODO: this is on sample data, it would become more appealing as spanid's grew over running length of application.*
   - perhaps a unit larger than 1. current data this is slightly worse overall 0.14%. it is now a constant that can be set. Still defaulting to 1.


---

add stride ie. linear prediction with encoding
   - stride to be stored as same custom float, with unit 1, representing diff across whole buffer.
   - this requires rebuilding values at compact time. which complicates implementation
   - metadata is simple. just add another custom 16bit float for the delta across the whole block 
     - not single row: spanids can repeat and total delta can be less than the number of rows. storing stride over whole block means we can store an *integer*

NOTE: linear extension and clamping.
we dont want to store first and last row for a call/arch, or have to look it up
instead we chose equivilent offset + stride which cover the whole block.
*clamping*: consider that if the first row that uses call/arch X for parent is N and has span index 0, the implied first spanindex would be negative. We don't want to store that. so we should clamp the offset to zero. This will slightly worsen the accuracy, for earlier entries in the frame. 

exact steps. 
1. take first and last rows which use give call/arch, R1 and R2, and their span indices S1 S2
2. take line defined by these points, see where it intersects R=0, this is the offset. if this is negative then replace line with one passing through R2, S2 (ie last) and 0,0
3. look at where line intersets R=L (frame length). The corresponding S minus offset is your stride.
4. to within precision: stride x R2 / L + offset = S2 and if we did not clamp to zero stride * R1 /L + offset = S1
5. if stride would be negative we set stride to 0, if S1=S2 we set stride to zero, this gracefully falls back to offset encoding

codex impl: saves 5.42%

| Storage component | Before (B) | After (B) | Change (B) |
|---|---:|---:|---:|
| Parent SpanId row fields | 1,030,043.250 | 608,178.375 | −421,864.875 |
| Other payload + packed padding | 5,620,037.750 | 5,619,809.625 | −228.125 |
| Parent end tables | 112,690 | 135,228 | +22,538 |
| Container metadata | 613,239 | 612,934 | −305 |
| **Total archive** | **7,376,010** | **6,976,150** | **−399,860** |


# strided (linear predicted) timestamp 
before cleaning up the stride impl let's add stride for timestamps as well. 

the stride can be stored with the same custom 16bit float
- max value is 4T or ~4400 seconds at ns resolution. 
- if this is exceeded, we will store with stride 0 (disabled)
- NOTE: because users might greatly increase the ts shift, we want to be able to accurately store small strides. a block with a duration of 50ms and 1ms ticks (shift 20) would have a stride of 50.

ALSO: add implicit leading 1 to mantisa of the float, unless exp=0 (to preserve small str handling)

- store ts stride with CollocatedMetadata, which does not grow due consuming existing alignment padding. 
- no change to Descriptor (already stores start ts)
  
| Component | Before KiB | After KiB | Change KiB | Change |
|---|---:|---:|---:|---:|
| **Archive overall** | **6,812.65** | **6,592.98** | **−219.67** | **−3.22%** |
| Span payload | 2,682.23 | 2,570.81 | −111.41 | −4.15% |
| Event/log payload | 3,531.85 | 3,423.41 | −108.45 | −3.07% |
| Metadata overhead | 598.57 | 598.76 | +0.19 | +0.03% |
| Start timestamps, combined | 2,666.75 | 2,449.98 | −216.77 | −8.13% |

TODO review codex changes

# overhead reduction
Added [Unalign<T, A> (line 74)](/home/user/files/projects/dev/util/tracing_table/src/data/unalign.rs:74) with safe get, set, replace, and into_inner methods. Used it for StringBufferId and the SmallBox pointer.
The fixed archive header shrank 26 → 22 bytes, saving 34,392 bytes on the reference capture without changing payloads.

# predictive string indices
store string indices as residual on offset+row, since often there is one string per row.

Adds one u16 per string field/arg to frame footer.

| Storage | Before | After | Change |
|---|---:|---:|---:|
| String indices¹ | 746.920 | 645.935 | **−100.984 (−13.52%)** |
| Predictor footer | 0 | 3.092 | +3.092 |
| Container metadata | 565.175 | 565.146 | −0.028 |

how many fallback to raw storage b/c its smaller **most!**
| Template extraction | Prediction on | Prediction off, varying | Constant | Total |
|---|---:|---:|---:|---:|
| Enabled | **528 — 7.87%** | **5,658 — 84.37%** | **520 — 7.75%** | 6,706 |
| Disabled | **724 — 15.93%** | **3,108 — 68.38%** | **713 — 15.69%** | 4,545 |

---

Fri Oct 23 10:25:12 PM EDT 2026

it seems that many callsites have cycles in their string indices

>37 callsites have an exact cycle in at least one archived string column, out of 87 callsites with varying string indices, with templates enabled.
> I counted a cycle when a nonconstant pattern repeats at least twice and matches the entire block exactly.
> 1,783 of 6,186 varying block/column pairs are cyclic (28.8%).
> 26 callsites are cyclic in at least half their varying block/column pairs.
> Allowing up to 5% mismatches raises the count to 39 callsites.
> Examples include render-context systems (period 41), compute dispatch (85), bind-group setting (28), and vertex-buffer setting (16).
> With templates disabled, 27 callsites have exact cycles.

we store a one byte period and one byte phase in addition to the 2 byte offset
we indicate noncyclic with period = 0

| Storage | Nominal (KiB) | Bits/column value | Change | Share of archive after |
|---|---:|---:|---:|---:|
| String indices | 646.248 → 579.073 | 5.134 → 4.600 | −10.39% | 9.105% |
| Predictor footer | 2.577 → 12.309 | 0.020 → 0.098 | +377.61% | 0.194% |
| **Overall archive** | **6,417.023 → 6,359.602** | — | **−0.89%** | **100%** |

predictive footer grows becaues many callsites that previously fell back to raw now use cyclic encoding.

Across all archived blocks, counting each string field/argument once per block, with templates enabled:

| Encoding | Count | Payload (KiB) | Bits/column value | % of archive |
|---|---:|---:|---:|---:|
| Cycle | 1,295 | 73.249 | 3.305 | 1.152% |
| Offset | 512 | 9.622 | 0.839 | 0.151% |
| Raw, varying | 4,379 | 506.478 | 6.150 | 7.964% |
| Raw, constant | 520 | 2.031 | 0.205 | 0.032% |
| **Total indices** | **6,706** | **591.381** | **4.698** | **9.299%** |

---

dictionarys.

up to 64 values is 6 bit address. the values are minimum of 6 bits to be unique 

at N rows direct storage is at least: Nx6 + VxVb + 6(value bit count up to 64 values) + 6(values count)
vs direct: NxVb

| Measure | Before | Now | Change |
|---|---:|---:|---:|
| Overall archive | 6,512,232 B | 6,443,134 B | **−69,098 B (−1.06%)** |
| Payload, including strings and end tables | 5,933,620 B | 5,865,047 B | **−68,573 B (−1.16%)** |
| Packed string-index bits | 4,526,831 | 3,651,640 | **−19.33%** |
| Full index storage, including masks and footers | 591.38 KiB | 524.31 KiB | **−11.34%** |

examples of string id encoding modes

| Encoding | Callsite | Messages |

|---|---|---:|
| Raw | **70:** `adjusting {} handle [{}] -> [{:?}]` | 59,817 |
| Offset residual | **137 / 283:** bind-group creation / drop — tied | 16,740 each |
| Cycle residual | **210:** `RenderContextState::apply` — span, field `system` | 100,448 |
| Dictionary | **70:** `adjusting {} handle [{}] -> [{:?}]` | 60,445 |
| Dictionary + cycle | **238:** `set bind_group {}: {:?} ({:?})` | 8,949 |
| Raw, constant | **223 / 268:** `Drop {}` for CommandEncoder / CommandBuffer — tied | 14,136 each |

supporting cycles in dictionary did not do much
> added two bytes to the footer entry for offset and period

| Component | Change |
|---|---:|
| Packed row payload | −10,096 B |
| Footers | +4,098 B |
| **Net archive** | **−5,998 B (−0.093%)** |

NOTE: this feature overall doesn't do much. string indices are already ~4bits average for templates. But dictionaries are a feature we want to support regardless.

TODO: cleanup + merge offset, cycle and dictionary encoding into a Column struct

# better string dedup

We could 1/2 string storage with global dedup.

| Arrangement | HC3 compressed bytes |
|---|---:|
| Original buffers | 1,129,976 |
| Concatenated | 996,029 |
| **Deduplicated across blocks within each archetype, then concatenated** | **868,289** |
| Globally deduplicated, then concatenated | 583,266 |

Most (98%) of the cross arch sharing is within crates (module top level) not between.

IDEA: one buffer collection for each crate. 
- **problem** we drop by chunk/buffer which is currently associated. if we did this, fast and slow logs get mixed into same string buffer. 
  - would require total redesign of string storage: reference counting and indirection for moving strings without changing ids and so on.

IDEA: for per arch cross buffer dedup: store a special buffer which contains any strings shared between two consequtive buffers. 
- *complex* and theoretical max gain of ~10-20%

Not really worth it.

# more clever fingerprinting of columns
Lets test how many unique strings columns actually have.

| Distinct values | Fields/args | String occurrences |
|---|---:|---:|
| 1 | 48 | 7.9% |
| 2–16 | 50 | 29.4% |
| 17–255 | 36 | 24.9% |
| 256+ | 38 | 37.8% |

IDEA: try to enumize every field and then switch to string buffer after 256 values.

# block dropping
TODO make dropping blocks use only archived blocks in usage calculation

# string conversion
IDEA: convert any numeric strings to numbers.

Hypothesis: conversion is actually worse because strings are deduped and compressed. 

Testing allowing u64 is a wash: payload actually increases, strings decrease, metadata a mixed bag. 

With u16.

| Storage | Before, B | After, B | Change |
|---|---:|---:|---:|
| **Total** | **12,164,755** | **12,052,159** | **−112,596 (−0.93%)** |
| Metadata | 1,198,444 | 1,139,418 | −59,026 |
| Row payload | 6,315,113 | 6,301,495 | −13,618 |
| Strings | 3,063,907 | 3,045,710 | −18,197 |
| Unused capacity | 1,587,291 | 1,565,536 | −21,755 |

CONCLUSION: this is too finicky to bother with. better direction is patching tracing. **I have reverted numeric string conversion, as well as dictionaries, cycles, and string cardinality metrics.** They are preserved in experiment/numeric-u16-2026-10-24 commit d596c59

# patching tracing and log
1. [x] patch tracing to emit callsites directly 
2. [x] store templates directly
3. [x] look up source code text (nice for display/debug) 
4. [x] patch logging to redirect to tracing
   - [ ] identify what does not work and falls back to log
     - dynamic keys in kv
     - explicit `logger:` (shouldn't redirect anyway)
5. [ ] replace fmt::Arguments with something inspectible