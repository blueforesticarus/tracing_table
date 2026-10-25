# Actix Web reference capture

Run from the repository root:

```sh
cargo run --release --locked --manifest-path examples/actix_capture/Cargo.toml
```

This standalone example starts a real Actix Web server on an ephemeral loopback
port, installs [`tracing-actix-web::TracingLogger`](https://docs.rs/tracing-actix-web/latest/tracing_actix_web/),
and drives concurrent HTTP requests with `awc`. It records all tracing targets at
TRACE and bridges `log` records, including HTTP codec and server diagnostics.
HTTP dependencies stay outside root library builds.

Traffic mixes readiness probes (10%), catalog reads (50%), orders (30%), and
reports (10%). Handlers simulate cache lookups, database pages and async I/O,
perform CPU transformations on blocking workers, and log response encoding and
outbox activity. Request spans retain parents across async and blocking work.
Some requests produce intentional inventory conflicts (409) and upstream errors
(503); the driver checks every response status and consumes every body.
There are no external databases, queues, or services.

The recorded timeline spans approximately **10 minutes** without waiting that
long. The driver advances the capture clock between drained request batches,
distributing idle gaps according to progress toward the byte target. It waits
for all captured spans to close before each jump, preserving real request and
child-span durations. A final request starts at the 10-minute mark; its normal
execution and server shutdown add a small tail. The optional live storage
comparison uses the same simulated clock.

The default output is `doc/actix_reference.msgpack.lz4`. An optional positional
argument selects another path. Default paths resolve from the repository, regardless
of the working directory. Both examples share capture setup, size validation and
reporting in `examples/support/capture.rs`; HTTP load generation lives in `driver.rs`,
and instrumented service stages live in `workload.rs`.
Like the Bevy reference, the capture stops at
**at least 100 MiB of MessagePack before LZ4**, finishing the current request
batch and shutting down the server before writing the completeness footer.
It does not pad or duplicate records to reach the target. The decoded fixture is
validated for size, parented events, and one completed middleware span per request. A `.capture.txt`
sidecar records exact sizes, request/status counts, settings and callsite families.

Tuning variables:

| Variable | Default | Purpose |
| --- | ---: | --- |
| `TRACING_CAPTURE_MIB` | 100 | Minimum MessagePack size, before LZ4 |
| `TRACING_CAPTURE_CONCURRENCY` | 32 | Concurrent requests per drained batch |
| `TRACING_CAPTURE_WORKERS` | 4 | Actix server workers |
| `TRACING_CAPTURE_DURATION_SECS` | 600 | Simulated timeline through the final request's start |
| `TRACING_CAPTURE_BATCHES` | 8 | CPU partitions and mock database pages per request |
| `TRACING_CAPTURE_CPU_ROUNDS` | 4096 | CPU mixing iterations per partition |
| `TRACING_CAPTURE_IO_MICROS` | 100 | Mock cache/database delay (runtime timer resolution applies) |
| `TRACING_CAPTURE_MAX_REQUESTS` | 1000000 | Fail if the target is not reached by this limit |

All settings must be positive integers; `TRACING_CAPTURE_MAX_REQUESTS` must be at
least 2 and includes the final request. For quick iteration:

```sh
TRACING_CAPTURE_MIB=1 cargo run --release --locked \
  --manifest-path examples/actix_capture/Cargo.toml -- /tmp/actix-smoke.msgpack.lz4
```

The default [storage pipeline](../../doc/storage_chart.md) replays both
`doc/bevy_reference.msgpack.lz4` and `doc/actix_reference.msgpack.lz4`. Their
measurements share a CSV with `run=bevy` / `run=actix`; chart populations combine
both while retaining independent callsite and block identities.
