//! Compare storage layouts on one captured stream of Bevy and generated logs.
//! Each requested block size replays the same rows and ignores its final partial block.
#[path = "../inspection.rs"]
mod inspection;
#[path = "../scene.rs"]
mod scene;

use bevy::{
    a11y::AccessibilityPlugin,
    asset::AssetPlugin,
    diagnostic::DiagnosticsPlugin,
    input::InputPlugin,
    prelude::*,
    time::TimeUpdateStrategy,
    transform::TransformPlugin,
    window::WindowPlugin,
    winit::{WinitPlugin, WinitSettings},
};
use lz4_flex::block::compress;
use std::{
    env,
    fmt::Write as _,
    fs,
    mem::size_of,
    sync::Arc,
    time::{Duration, Instant},
};
use streaming_iterator::StreamingIterator;
use tracing_table::{
    data::dataframe::{CompactFrame, Frame},
    subscriber::RecordingLayer,
};

const BLOCK_BYTES: [usize; 3] = [4 * 1024, 64 * 1024, 256 * 1024];
const HEADER_BYTES: usize = 2 * size_of::<usize>();

#[derive(Clone)]
struct ResultRow {
    workload: &'static str,
    block_bytes: usize,
    target: String,
    name: String,
    callsite: u64,
    archetype: u32,
    unit: usize,
    total_rows: usize,
    blocks: usize,
    raw: usize,
    compact: usize,
    lz4: usize,
    compact_lz4: usize,
}

impl ResultRow {
    fn label(&self) -> String {
        format!(
            "{}::{} [callsite {}, archetype {}, {} B/row, {} blocks]",
            self.target, self.name, self.callsite, self.archetype, self.unit, self.blocks
        )
    }

    fn sizes(&self) -> [usize; 3] {
        [self.compact, self.lz4, self.compact_lz4]
    }
}

fn setting(name: &str, default: usize) -> usize {
    let value = env::var(name)
        .map(|value| {
            value
                .parse::<usize>()
                .expect("benchmark setting must be an integer")
        })
        .unwrap_or(default);
    assert!(value > 0, "{name} must be positive");
    value
}

fn emit_generated(requests: usize) {
    const ROUTES: [&str; 6] = [
        "/api/items",
        "/api/users",
        "/api/search",
        "/api/orders",
        "/health",
        "/api/feed",
    ];
    const QUERIES: [&str; 4] = [
        "select item by id",
        "select user profile",
        "update order",
        "list recent items",
    ];
    // A fixed seed keeps the values repeatable while preserving realistic hot and cold paths.
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    for request in 0..requests {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let route = ROUTES[(rng as usize) % ROUTES.len()];
        let client_id = (rng >> 17) % 8_000;
        let request_id = rng;
        let status = if request % 151 == 0 { 503_u64 } else { 200_u64 };
        let duration_us = 700 + ((rng >> 27) % 25_000);
        let bytes = 200 + ((rng >> 41) % 12_000);
        let request_span = tracing::info_span!(
            target: "generated::http", "request", request_id, route, client_id
        );
        let _entered = request_span.enter();
        tracing::debug!(
            target: "generated::router", route, method = "GET", client_id,
            message = "request routed"
        );
        if request % 5 != 0 {
            tracing::trace!(
                target: "generated::database", query = QUERIES[request % QUERIES.len()],
                rows = (rng >> 8) % 120, duration_us = duration_us / 3,
                message = "query completed"
            );
        }
        tracing::info!(
            target: "generated::response", route, status, duration_us, bytes,
            message = "request completed"
        );
        if status >= 500 {
            tracing::warn!(
                target: "generated::errors", route, status, error_code = request % 7,
                message = "upstream unavailable"
            );
        }
        if request % 32 == 0 {
            tracing::debug!(
                target: "generated::queue", partition = request % 12,
                offset = request, batch_size = 10 + request % 200,
                message = "batch acknowledged"
            );
        }
    }
}

#[derive(Resource)]
struct FramesLeft(usize);

fn exit_after_frames(mut frames: ResMut<FramesLeft>, mut commands: Commands) {
    frames.0 -= 1;
    if frames.0 == 0 {
        commands.write_message(AppExit::Success);
    }
}

fn run_bevy(frames: usize, windowed: bool) {
    let mut app = App::new();
    // Run the real scene and Bevy's non-rendering library systems. PBR systems
    // require a RenderDevice, which Bevy cannot create without a window here.
    app.add_plugins(MinimalPlugins)
        .add_plugins((
            AssetPlugin::default(),
            TransformPlugin,
            DiagnosticsPlugin,
            InputPlugin,
        ))
        .init_resource::<Assets<Mesh>>()
        .init_resource::<Assets<StandardMaterial>>()
        .init_resource::<Assets<Image>>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 30.0,
        )))
        .init_resource::<scene::SceneSettings>()
        .add_systems(Startup, scene::setup)
        .add_systems(
            Update,
            (
                scene::advance_log_clock,
                scene::move_cube,
                scene::rotate_cube,
                scene::scale_center,
            )
                .chain(),
        );
    if windowed {
        // Winit needs an OS event loop, so let it drive the same scene for a
        // finite number of frames. The renderer stays off for Xvfb support.
        app.add_plugins((
            WindowPlugin::default(),
            AccessibilityPlugin,
            WinitPlugin::default(),
        ))
        .insert_resource(WinitSettings::continuous())
        .insert_resource(FramesLeft(frames))
        .add_systems(Last, exit_after_frames);
        assert_eq!(app.run(), AppExit::Success);
    } else {
        for _ in 0..frames {
            app.update();
        }
    }
}

fn measure_block(rows: &[u8], unit: usize, count: usize) -> [usize; 4] {
    let mut frame = Frame::new(unit, count);
    for row in rows.chunks_exact(unit) {
        frame.push(row);
    }
    assert_eq!(frame.length, count);
    let compact = CompactFrame::new_from(&frame);
    let changing = compact.unit - compact.mask().constant_bytes();
    let initialized_len = 2 * unit + (count - 1) * changing;
    // CompactFrame reserves one extra compact row but does not initialize it.
    // Give that allocated tail defined zero bytes before feeding the *entire*
    // CompactFrame::buffer to LZ4. This preserves its measured buffer length.
    let mut compact_buffer = vec![0_u8; compact.buffer.len()];
    compact_buffer[..initialized_len].copy_from_slice(&compact.buffer[..initialized_len]);
    [
        HEADER_BYTES + frame.buffer.len(),
        HEADER_BYTES + compact.buffer.len(),
        HEADER_BYTES + compress(&frame.buffer).len(),
        HEADER_BYTES + compress(&compact_buffer).len(),
    ]
}

fn gather(recorder: &RecordingLayer) -> Vec<ResultRow> {
    let mut results = Vec::new();
    let callsites = recorder.callsites.read();
    for callsite in callsites.values() {
        let data = callsite.read();
        for arch in &data.cache {
            let unit = arch.archetype.unit();
            if unit == 0 {
                continue;
            }
            // Copy one archetype once, including the active tail, so every block
            // size and compression mode sees exactly the same recorded row bytes.
            let mut rows = Vec::new();
            let mut iter = arch.data.iter();
            while let Some(row) = iter.next() {
                rows.extend_from_slice(row);
            }
            assert_eq!(rows.len() % unit, 0);
            let row_count = rows.len() / unit;
            let target = data.metadata.target().to_owned();
            let workload = if target.starts_with("generated::") {
                "generated"
            } else if target.contains("::scene") {
                "bevy_scene"
            } else {
                "bevy_library"
            };
            for block_bytes in BLOCK_BYTES {
                let block_rows = block_bytes / unit;
                if block_rows == 0 {
                    continue;
                }
                let full_blocks = row_count / block_rows;
                if full_blocks == 0 {
                    continue;
                }
                let mut sizes = [0_usize; 4];
                for block in rows[..full_blocks * block_rows * unit].chunks_exact(block_rows * unit)
                {
                    let block_sizes = measure_block(block, unit, block_rows);
                    for (total, part) in sizes.iter_mut().zip(block_sizes) {
                        *total += part;
                    }
                }
                results.push(ResultRow {
                    workload,
                    block_bytes,
                    target: target.clone(),
                    name: data.metadata.name().to_owned(),
                    callsite: data.id,
                    archetype: arch.id,
                    unit,
                    total_rows: row_count,
                    blocks: full_blocks,
                    raw: sizes[0],
                    compact: sizes[1],
                    lz4: sizes[2],
                    compact_lz4: sizes[3],
                });
            }
        }
    }
    results.sort_by(|a, b| {
        (a.workload, a.block_bytes, &a.target, &a.name, a.archetype).cmp(&(
            b.workload,
            b.block_bytes,
            &b.target,
            &b.name,
            b.archetype,
        ))
    });
    results
}

fn escape_csv(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn report(rows: &[ResultRow], path: &str) {
    let mut csv = String::from(
        "workload,block_bytes,target,name,callsite,archetype,unit,total_rows,full_blocks,raw,compact,lz4,compact_lz4\n",
    );
    for row in rows {
        writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            row.workload,
            row.block_bytes,
            escape_csv(&row.target),
            escape_csv(&row.name),
            row.callsite,
            row.archetype,
            row.unit,
            row.total_rows,
            row.blocks,
            row.raw,
            row.compact,
            row.lz4,
            row.compact_lz4,
        )
        .unwrap();
    }
    fs::write(path, csv).expect("write detailed compression report");
    println!("Archetype report: {path}");
    for workload in ["bevy_library", "bevy_scene", "bevy", "generated", "all"] {
        for block_bytes in BLOCK_BYTES {
            let selected: Vec<_> = rows
                .iter()
                .filter(|row| {
                    row.block_bytes == block_bytes
                        && (workload == "all"
                            || row.workload == workload
                            || (workload == "bevy" && row.workload.starts_with("bevy_")))
                })
                .collect();
            if selected.is_empty() {
                continue;
            }
            let totals = selected.iter().fold([0_usize; 4], |mut total, row| {
                for (sum, size) in
                    total
                        .iter_mut()
                        .zip([row.raw, row.compact, row.lz4, row.compact_lz4])
                {
                    *sum += size;
                }
                total
            });
            let blocks: usize = selected.iter().map(|row| row.blocks).sum();
            println!(
                "\n{workload} {block_bytes} B: {} archetypes, {blocks} full blocks; raw={} compact={} lz4={} compact+lz4={}",
                selected.len(),
                totals[0],
                totals[1],
                totals[2],
                totals[3]
            );
            for (mode, index) in [("compact", 0), ("lz4", 1), ("compact+lz4", 2)] {
                // Cross multiplication orders ratios without rounding ties.
                let compare = |a: &&ResultRow, b: &&ResultRow| {
                    ((a.sizes()[index] as u128) * (b.raw as u128))
                        .cmp(&((b.sizes()[index] as u128) * (a.raw as u128)))
                };
                let best = selected.iter().copied().min_by(compare);
                let worst = selected.iter().copied().max_by(compare);
                let savings = 100.0 * (1.0 - totals[index + 1] as f64 / totals[0] as f64);
                println!("  {mode}: {savings:.2}% saved overall");
                if let (Some(best), Some(worst)) = (best, worst) {
                    println!(
                        "    best:  {:.2}% {}",
                        100.0 * (1.0 - best.sizes()[index] as f64 / best.raw as f64),
                        best.label()
                    );
                    println!(
                        "    worst: {:.2}% {}",
                        100.0 * (1.0 - worst.sizes()[index] as f64 / worst.raw as f64),
                        worst.label()
                    );
                }
            }
        }
    }
}

fn main() {
    let bevy_frames = setting("TRACING_COMPRESSION_BEVY_FRAMES", 16_000);
    let generated_requests = setting("TRACING_COMPRESSION_REQUESTS", 60_000);
    let windowed = env::var("TRACING_COMPRESSION_BEVY_WINDOWED")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    let output = env::var("TRACING_COMPRESSION_REPORT")
        .unwrap_or_else(|_| "/tmp/tracing_compression_report.csv".to_owned());
    let recorder = Arc::new(RecordingLayer::new());
    // This is the same retention wrapper used by the viewer, so borrowed Atom
    // handles in historical rows stay valid throughout the benchmark.
    tracing::subscriber::set_global_default(inspection::subscriber(recorder.clone()))
        .expect("install recorder before Bevy startup");
    tracing_log::LogTracer::init().expect("bridge log records into tracing");
    let started = Instant::now();
    run_bevy(bevy_frames, windowed);
    println!(
        "Bevy capture ({}): {bevy_frames} frames in {:?}",
        if windowed { "windowed" } else { "headless" },
        started.elapsed()
    );
    let started = Instant::now();
    emit_generated(generated_requests);
    println!(
        "Generated capture: {generated_requests} requests in {:?}",
        started.elapsed()
    );
    let started = Instant::now();
    let rows = gather(&recorder);
    println!("Measurement: {:?}", started.elapsed());
    report(&rows, &output);
}
