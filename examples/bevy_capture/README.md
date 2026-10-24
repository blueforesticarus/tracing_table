# Bevy reference capture

This standalone example renders an animated scene offscreen and records detailed
Bevy and wgpu tracing into the same MessagePack/LZ4 format as the
[Actix capture](../actix_capture/README.md). No display or interactive UI is needed;
a working GPU or software graphics driver is required.

```sh
make capture-bevy
# Or run directly:
cargo run --locked --manifest-path examples/bevy_capture/Cargo.toml
```

The default output is `doc/bevy_reference.msgpack.lz4`, resolved from the repository
regardless of working directory. An optional positional argument selects another
path. Detailed tracing and UI rendering are enabled directly in the manifest.
The UI consists of changing scene text to exercise Bevy's layout and rendering.

The capture targets at least **100 MiB of MessagePack before LZ4** and at least
120 rendered frames. It waits for GPU submissions, verifies the local glTF scene
loaded, and shuts down the app before finishing and validating the dump. A
`.capture.txt` sidecar records settings, adapter, sizes and callsite families.

| Variable | Default | Purpose |
| --- | ---: | --- |
| `TRACING_CAPTURE_MIB` | 100 | Minimum MessagePack size before LZ4 |
| `TRACING_CAPTURE_MIN_FRAMES` | 120 | Minimum rendered frames |
| `TRACING_CAPTURE_MAX_FRAMES` | 10000 | Fail if the target is not reached by this limit |
| `TRACING_CAPTURE_WIDTH` | 320 | Offscreen image width |
| `TRACING_CAPTURE_HEIGHT` | 180 | Offscreen image height |
| `TRACING_CAPTURE_PROBES` | 4 | Transform-log samples per mesh per frame |

All values must be positive. Minimum frames cannot exceed maximum frames.
For a smaller validation run:

```sh
TRACING_CAPTURE_MIB=1 cargo run --locked \
  --manifest-path examples/bevy_capture/Cargo.toml -- /tmp/bevy-smoke.msgpack.lz4
```

`src/main.rs` contains renderer setup and the finite capture loop. Procedural
scene assets and motion live in `src/scene.rs`; animation, UI and additional
logging live in `src/workload.rs`. Capture IO and reporting are shared through
`examples/support/capture.rs`. The local glTF fixture lives in `assets/`.

The [storage pipeline](../../doc/storage_chart.md) computes CSV and charts from
both reference dumps by default.
