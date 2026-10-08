## Bevy tracing viewer

The standalone example adapts Bevy's [Transform scene](https://bevy.org/examples/transforms/transform/)
with several animated cubes and an egui tracing overlay. It installs the recording
subscriber before app startup, disables Bevy's subscriber setup, and bridges
`log` events into tracing. All targets are captured at TRACE, including worker
threads. Recording and compaction remain synchronous on the emitting thread.

```sh
cargo run --manifest-path examples/bevy_viewer/Cargo.toml
```

Bevy 0.19 and bevy_egui 0.42 belong only to that standalone package. Root builds,
tests, and the root synthetic benchmark do not compile them. The existing nightly Rust toolchain
is required. The viewer needs a display and a working GPU/software graphics driver.

This example’s subscriber
wrapper retains unique string atoms per callsite so old log fields remain live.
Recording and retention share a per-callsite lock to handle concurrent emitters;
these example-owned pools are excluded from compression accounting.

Stats refresh every 500 ms. Select an archetype to inspect its fields, compression,
and newest matching records. Search the archetype table by target, source, ID,
record kind, or field name. Enter an exact `entity` field value to filter records:
use the displayed numeric ID for scene entities, or `str:` followed by the exact
string/debug representation for an engine entity. Blank includes all records.
Only a record's own entity field matches; parent spans are not consulted. Filtering
pages backwards through history to find the newest matches, rather than filtering
just the latest unfiltered page.

**Dump MessagePack** saves all currently recorded data to a uniquely named
`tracing_dump_*.msgpack` file in the current working directory. The viewer shows
the saved path or any write error below the button.

The root [dump compression example](../dump_messsage_compression/README.md) reads the
dump as a stream and prints aggregate CSV tables. Storage and duplicate-byte
metrics default to bytes per original matched row, with dictionary and message
payload reported separately. Comma-separated CLI dimensions
cover deduplication, row block size, field group, Zstd level, dictionary size,
and training sample size:

```sh
cargo run --release --example dump_message_compression -- \
  --dedup=plain,block,archetype,global --block_size=100,1000 \
  --fields=message,other --timing --sizes --summary path/to/dump.msgpack
cargo run --release --example dump_message_compression -- --list_targets path/to/dump.msgpack
cargo run --release --example dump_message_compression -- --dump_strings=messages.tsv path/to/dump.msgpack
```

Use `--target='glfw*'` to filter by target glob, or `--target=corngame` to
include a target and its child modules. Plain/block dictionary samples concatenate
whole messages to a target of 256 bytes by default. Archetype/global dictionaries
train on individual unique messages and store each as raw or compressed,
whichever is smaller. See the example's documentation
for table definitions, tuning options, and further commands.
Use `--dedup=block --prefix=off,on --compression_mode=blocks` to compare longest
common block-prefix splitting before and after regular compression. Prefix mode
supports only message fields with dedup; prefix + dictionary compression is discouraged.
`--dump_strings` streams original message occurrences to a new TSV file with
unique archetype ID, target, and message columns for further prefix exploration.

The Tuning controls adjust refresh interval, matching-row limit (default 1,000),
UI scale, scene log rate, and scene motion/scaling. Each archetype has an example-owned
running total for completed frames. Unchanged packed block lengths reuse their stats;
new or edited completed frames replace their previous contributions. Only the
active frame is recomputed on every refresh. No aggregation or entity queries are
implemented in the library. Storage is not evicted, so TRACE runs accumulate data.

`data` is the row size before frame compaction, **after string interning**.
`compressed` counts packed row bytes; `overhead` estimates masks and frame/cache
headers; `overalloc` counts unused frame capacity. Interned string allocations,
subscriber maps, and viewer caches are excluded. Compression shown for selected
archetypes describes their whole shared frames, even when the displayed logs are
filtered by entity.

For a finite display/GPU smoke run:

```sh
TRACING_VIEWER_SMOKE_FRAMES=120 cargo run --manifest-path examples/bevy_viewer/Cargo.toml
```

## Full-block compression comparison

This binary runs the same scene systems headlessly by default, captures Bevy
library callsites with Bevy's opt-in `detailed_trace` feature, and emits a separate
HTTP/database/queue workload. Set `TRACING_COMPRESSION_BEVY_WINDOWED=1` when an
X display is available to include `winit`/window-event callsites without starting
the renderer. It records each workload once. For each archetype,
the recorded rows are replayed into 4 KiB, 64 KiB, and 256 KiB frame capacities;
only full frames at each capacity enter that capacity's comparison.

```sh
cargo run --release --manifest-path examples/bevy_viewer/Cargo.toml \
  --bin compression_bench --features compression_detailed_trace

# Include winit/window event logs when an X display is available:
TRACING_COMPRESSION_BEVY_WINDOWED=1 cargo run --release \
  --manifest-path examples/bevy_viewer/Cargo.toml \
  --bin compression_bench --features compression_detailed_trace
```

`TRACING_COMPRESSION_BEVY_FRAMES` (default 16,000) and
`TRACING_COMPRESSION_REQUESTS` (default 60,000) control the sample length.
`TRACING_COMPRESSION_REPORT` selects the detailed CSV path (default
`/tmp/tracing_compression_report.csv`). The program prints weighted totals and
best/worst full-block archetypes for Bevy libraries, the scene, the generated
workload, and their combinations. The CSV contains every archetype with at least
one full block at a given size.

All four byte counts include the `unit` and `length` fields (two `usize`s):
uncompressed counts the entire `Frame::buffer`, compact counts the entire
`CompactFrame::buffer`, LZ4 compresses `Frame::buffer`, and compact plus LZ4
compresses `CompactFrame::buffer`. `CompactFrame` allocates one spare compact row
without initializing it; the benchmark supplies zero bytes for that tail before
LZ4 so the entire buffer can be compressed reproducibly. String allocations and
per-archetype/cache structures are outside these four frame-size counts.

The headless app uses Bevy's core, asset, transform, diagnostics, and input
plugins. The windowed mode adds Bevy's `WindowPlugin` and `WinitPlugin` with a
continuously updating event loop. Neither mode runs the renderer, so rendering
callsite traffic is outside this comparison. The regular viewer does not enable
`detailed_trace` unless explicitly requested.

## Synthetic tracing benchmark

This ignored integration test emits fast workloads from many static callsites,
with multiple threads, deterministic rates, repeated/changing strings, numeric
entity fields, and occasional spans. It checks event counts and decoded samples,
and reports emission throughput separately from final inspection time.

```sh
cargo test --release --test synthetic_benchmark -- --ignored --nocapture
TRACING_BENCH_THREADS=4 TRACING_BENCH_ITERATIONS=10000 TRACING_BENCH_CALLSITES=64 \
  cargo test --release --test synthetic_benchmark -- --ignored --nocapture
```

Defaults are up to 8 available CPU threads, 100,000 iterations per thread, and 32
callsites. `TRACING_BENCH_THREADS` supports 1–1,024; `TRACING_BENCH_CALLSITES` supports
1–64. Both emission and inspection measurements exclude recorder destruction.
Normal `cargo test` skips this workload but tests the example's cache/filter policy
without Bevy. Run example-package tests separately with:

```sh
cargo test --manifest-path examples/bevy_viewer/Cargo.toml
```

The archetype table shows rows, blocks, data, total, compressed, overhead,
overalloc, ratio, and raw ratio. Total is compressed + overhead + overalloc.
It defaults to descending compressed size. Click any heading to sort it, and
click again to reverse. Ratio measures savings including overhead and overalloc;
raw ratio measures row-byte savings from compaction alone. Blocks includes the
active block. **Bytes / row** divides the five byte totals by each archetype's
row count, and divides the overall summary by the overall row count.

**Indexed atoms** switches string storage and clears all recorded logs and cached
stats, including callsite schemas and retained string pools. Callbacks from spans
opened before the reset are ignored when they later update or close. The global
string index remains available to existing handles. Ctrl + **+** (or **=**) and
Ctrl + **-** adjust UI scale, also available in Tuning.

Drag the horizontal divider between the archetype table and inspection pane to
resize their split. Use **Select entity…** for entities found in the newest
unfiltered records of the selected archetype, or click an `entity=…` button on a
log row. **Clear entity** returns to all records. Exact text entry remains
available for entities outside the recent picker list.

Selected archetypes show a component table with field, type, sizeof, effective
size (packed / completed rows), data, packed, and ratio. It includes timestamp,
end timestamp, parent, follows-from, and span headers wherever stored. Prefix
strings have separate prefix and suffix rows in both storage modes. Each
component counts its full first-row bytes once per completed block, plus varying
byte positions for subsequent rows. Active blocks, frame metadata, masks, and
separately allocated interned strings are excluded. Changed packed block lengths
replace cached contributions; unchanged blocks reuse their previous figures.

All size displays use decimal K (1,000) and M (1,000,000), rounded to three
significant figures, with no suffix below K. Hover over archetype size cells for
exact byte counts.

The **Strings** tab contains a storage breakdown and a virtualized **Index / Count /
String** table. Click Index or Count to sort and click again to reverse; Count starts
with the most frequently constructed strings. Only visible rows are rendered.
Hover over truncated strings for their full text, and adjust string column width in
Tuning. Refresh strings updates immediately; automatic updates use the same refresh
interval as logs.

The breakdown shows unique text, counted handles, Atom entry metadata, fixed Atom
table storage, the forward index map and its spare capacity, and an occupied-bucket
estimate for the reverse map. The public API does not expose the reverse map’s
spare capacity or diagnostic counts-map allocation. Counts are cumulative indexed constructions, not live references;
inline strings and non-indexed Atom constructions are not counted. The global index
survives log resets. Maximum index and the byte width required by tagged IDs help
investigate field compaction. A short inline suffix can make all eight handle bytes
vary when mixed with indexed IDs in one block, despite a small maximum ID.

**Avg block duration** in the archetype table (also a row in selected-archetype
inspection) averages `max(ts) - min(ts)` across completed timestamped blocks.
Active blocks are excluded to avoid averaging partially filled blocks. Event rows
use event time; span rows use start time, excluding their mutable end time.
Span-update rows store a span reference instead of a timestamp and show “—”.
Durations restore the recorder's timestamp quantization and are cached along with
completed-frame stats. This metric reads row timestamps directly, rather than the
recorder's private block range metadata.

Timestamp ordering audit: `RecordingLayer::timestamp` uses elapsed `Instant` time,
so wall-clock corrections do not affect these timestamps. Quantization allows
identical timestamps. The core subscriber samples time before acquiring the
callsite write lock, so concurrent emitters can append rows out of timestamp order,
including across block boundaries. This example's retention wrapper serializes
event and span-start callbacks per callsite before sampling time; those row-start
timestamps should therefore be nondecreasing within an archetype. Different
archetypes/callsites cannot be ordered by row or block index. Resetting the viewer
clears rows without resetting the clock; separate recorder instances have different
clock origins.

Span end timestamps are not ordered by span start/row index: spans can close in
any order, and closing an old span expands an old block's timestamp range after
newer blocks exist. Span-close callbacks are not serialized by the retention pool.
Consequently block ranges can overlap, and their latest timestamps need not be
monotonic across blocks. The completed-span range lookup finds the containing block by searching backward
for a starting index less than or equal to the row index.

Span-start headers reserve **extra_archetype** and **extra_block**, each a `u32`.
On close, spans with updated fields receive the update archetype ID and a one-based
block ordinal; block zero means no extra fields. The reference remains valid when
an active update block becomes a completed block. Hover over **extra archetype:block**
on a log row to see its updated fields. The viewer scans only that referenced block
and matches the update row’s original SpanId; it does not search the full history.
The component table accounts for both reference halves separately. Late span closes
refresh compression masks when modifying previously constant header bytes.
