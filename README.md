This crate implements an memory-efficient cache for tracing logs, with support for per-callsite rollover. 

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

By default Timestamps are stored as u64 nanoseconds. However, 6 of those bytes represent days. A bit shift can be set to disregard the low bits, if they are noise. This is specified with the env var `RUST_LOG_TIMER_RES=<amount to shift>`

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
4. should allow per thread dumping to a collector thread
5. probably should have bounded lookups for spans active at a given time, which effects block design, might be able to get away without it due to events pointing to span id's. we already have bounded lookup for events/span start/end
6. we need indexing over fields, for efficient entity lookup
7. all sources of memory use must be tracked. 