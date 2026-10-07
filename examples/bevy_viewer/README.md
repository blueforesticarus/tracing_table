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
tests, and the benchmark do not compile them. The existing nightly Rust toolchain
is required. The viewer needs a display and a working GPU/software graphics driver.

This example uses the existing recorder implementation unchanged. Its subscriber
wrapper retains unique string atoms per callsite so old log fields remain live.
Recording and retention share a per-callsite lock to handle concurrent emitters;
these example-owned pools are excluded from compression accounting. Storage fixes,
including delayed span-close behavior, remain outside the example’s scope.

Stats refresh every 500 ms. Select an archetype to inspect its fields, compression,
and newest matching records. Search the archetype table by target, source, ID,
record kind, or field name. Enter an exact `entity` field value to filter records:
use the displayed numeric ID for scene entities, or `str:` followed by the exact
string/debug representation for an engine entity. Blank includes all records.
Only a record's own entity field matches; parent spans are not consulted. Filtering
pages backwards through history to find the newest matches, rather than filtering
just the latest unfiltered page.

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
monotonic across blocks. The core completed-span range lookup also uses the first
block with `index > block.index`, instead of the containing block: it can update
the wrong block and can panic for the first row of the first completed block.
This audit leaves the core implementation unchanged.
