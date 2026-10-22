THIS FILE UNFORTIONATELY MIXES CODEX GENERATED DOCS AND HUMAN DOCS

This crate implements an memory-efficient cache for tracing logs, with support for per-callsite rollover. 

Archived frames use bit compaction by default. Build with `--no-default-features`
to use byte compaction, or set `default-features = false` on the dependency.
Bit frames pack varying bits continuously across row boundaries by default.
Set `CONTINUOUS_BIT_PACKING` in `src/data/dataframe.rs` to `false` and rebuild
to restore byte-aligned packed rows. The constant does not affect byte compaction.
[Measured continuous-packing results](doc/continuous_packing_2026-10-22.md).

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

*TODO: End timestamps are relative to the start timestamp?*

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

1. implement our own string cache
   this needs to handle singleton and repeated strings more intelligently 
   it needs to not inline short strings if it prevents block compaction
   it likely should store seperate index tables for different lengths of string
   it possibly should consider how its design can facilitate prefix and mask representations
   the storage of strings itself should likely be blocks which can be compressed.
   must support deletion
2. support deletion of blocks, which must also cleanup strings
3. order of logs (in a thread, at a minimum, though possibly nominally) must be recoverable. timestamps are not enough
   what we should do, is use timestamp + disambiguator
   disambiguator SHOULD be cross thread so that cross thread timing can be reconstructed, since that timing may be nonarbitrary in the case of locks.
   we can reduce bits in order to 
4. we might want to allow blocks to grow past default length, untill adding the row would require invalidate a currently compactible bytecolumn
5. support bitpacked version of frame
6. support compressing frames.
7. should allow per thread dumping to a collector thread
8. probably should have bounded lookups for spans active at a given time, which effects block design, might be able to get away without it due to events pointing to span id's. we already have bounded lookup for events/span start/end
9.  we need indexing over fields, for efficient entity lookup
10. all sources of memory use must be tracked. 
11. we should probably compute the prefix per block, when compacting.
   though this may result in "finished frame 1000|1" "finished frame 1000|2"
   when what we want is "finished frame |10001"
   however whole archetype prefixing means one weird message disables prefixing entirely. 
   **compromise might be reseting when prefix is 0 (or a small number of bytes, 8 feels right due to overhead reasons)**
   but we still should resplit the block on compression
12. traitify dyntuple values. support multiple rep formats for a given output type. try to avoid depending on subscriber for soundness of dyntuple actions. may consider nestable / composable dyntuples.
13. need a test which compares just recording every log directly to a file with logs reconstructed from the tracing_table, for accuracy.
14. we may want to store the blocks columnar, either by field or by byte. This might assist with compression. supporting this is tricky, ideally should be a compiletime abstraction for perf
15. messages are generally similar per callsite, so it is likely they should be stored together per callsite, possibly in a compressed block. fields are more likely to be shared, so perhaps handle those seperately. compression could use lz4 with a dictionary shared by all the blocks of a given archetype.
16. however string fields are stored it needs to be easy to equality check, so we can scan for a value like entity="201v1", *this is different than message which has different tradeoffs*

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

The [benchmark standard](doc/benchmarking.md) describes streaming LZ4/MessagePack capture, deterministic replay with original timestamps, and per-callsite timing/storage reports.

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

![Archived storage pie charts](doc/bevy_reference_archived_storage.png)

[SVG](doc/bevy_reference_archived_storage.svg) · [Storage breakdown and methodology](doc/bevy_reference_archived_storage.md)
   
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