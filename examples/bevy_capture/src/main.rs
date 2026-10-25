#![recursion_limit = "256"]
//! Rendered, windowless reference capture with detailed Bevy and wgpu tracing.
#[path = "../../support/capture.rs"]
mod capture_support;
mod scene;
mod workload;
use bevy::{
    app::PluginsState,
    diagnostic::{EntityCountDiagnosticsPlugin, FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin},
    log::LogPlugin,
    prelude::*,
    render::{
        RenderPlugin,
        pipelined_rendering::PipelinedRenderingPlugin,
        render_resource::PollType,
        renderer::{RenderAdapterInfo, RenderDevice},
    },
    time::TimeUpdateStrategy,
    window::ExitCondition,
};
use capture_support::{Result, Session, callsite_families, setting};
use std::time::Duration;

struct Settings {
    minimum_frames: u64,
    max_frames: u64,
    scene: workload::CaptureSettings,
}
impl Settings {
    fn from_env() -> Result<Self> {
        let settings = Self {
            minimum_frames: setting("TRACING_CAPTURE_MIN_FRAMES", 120)?,
            max_frames: setting("TRACING_CAPTURE_MAX_FRAMES", 10_000)?,
            scene: workload::CaptureSettings {
                probes: setting("TRACING_CAPTURE_PROBES", 4)?.try_into()?,
                width: setting("TRACING_CAPTURE_WIDTH", 320)?.try_into()?,
                height: setting("TRACING_CAPTURE_HEIGHT", 180)?.try_into()?,
            },
        };
        if settings.minimum_frames > settings.max_frames {
            return Err(
                "TRACING_CAPTURE_MIN_FRAMES must not exceed TRACING_CAPTURE_MAX_FRAMES".into(),
            );
        }
        Ok(settings)
    }
}

fn main() -> Result<()> {
    // Export before initializing a renderer, server, or capture subscriber.
    if capture_support::export_callsites()? {
        return Ok(());
    }

    let settings = Settings::from_env()?;
    let session = Session::start("bevy")?;
    let mut app = build_app(settings.scene);
    let adapter = app.world().resource::<RenderAdapterInfo>();
    let adapter_report = format!(
        "adapter={} backend={:?} device_type={:?} driver={} {}",
        adapter.name, adapter.backend, adapter.device_type, adapter.driver, adapter.driver_info
    );
    eprintln!("{adapter_report}");
    let frames = record_frames(&mut app, &session, &settings)?;
    // Prove that the async glTF/scene path completed, rather than merely queuing a load.
    let world = app.world_mut();
    assert!(
        world
            .query::<&Name>()
            .iter(world)
            .any(|name| name.as_str() == "LoadedTriangle"),
        "glTF scene did not finish spawning"
    );
    // Capture renderer/asset shutdown and final span closes before the footer.
    drop(app);
    let fixture = session.finish()?;
    let families = callsite_families(&fixture);
    assert!(
        families.keys().any(|target| target.starts_with("wgpu")),
        "missing graphics traces"
    );
    assert!(
        families.contains_key("bevy_render"),
        "missing renderer traces"
    );
    session.write_report(
        &fixture,
        &format!(
            "{adapter_report}\nframes={frames}\n\
         minimum_frames={}\nmax_frames={}\n\
         probes={}\nwidth={}\nheight={}\n\
         features=Bevy trace, detailed_trace, offscreen 3D/UI\n",
            settings.minimum_frames,
            settings.max_frames,
            settings.scene.probes,
            settings.scene.width,
            settings.scene.height
        ),
    )
}

// Keep renderer/plugin setup apart from the finite capture loop.
fn build_app(settings: workload::CaptureSettings) -> App {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .build()
            .disable::<LogPlugin>()
            .disable::<PipelinedRenderingPlugin>()
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .set(AssetPlugin {
                file_path: concat!(env!("CARGO_MANIFEST_DIR"), "/assets").into(),
                ..default()
            })
            .set(RenderPlugin {
                synchronous_pipeline_compilation: true,
                ..default()
            }),
    )
    .add_plugins((
        FrameTimeDiagnosticsPlugin::default(),
        EntityCountDiagnosticsPlugin::default(),
        LogDiagnosticsPlugin::default(),
    ))
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )))
    .init_resource::<scene::SceneSettings>()
    .insert_resource(settings)
    .add_systems(Startup, (scene::setup, workload::setup).chain())
    .add_systems(
        Update,
        (
            scene::advance_log_clock,
            scene::move_cube,
            scene::rotate_cube,
            scene::scale_center,
            workload::update,
        )
            .chain(),
    );
    while app.plugins_state() != PluginsState::Ready {
        bevy::tasks::tick_global_task_pools_on_main_thread();
        std::thread::yield_now();
    }
    app.finish();
    app.cleanup();
    app
}

fn record_frames(app: &mut App, session: &Session, settings: &Settings) -> Result<u64> {
    let mut frames = 0;
    loop {
        tracing::info!(target: "tracing_table::capture_boundary", frame = frames + 1, phase = "begin", "frame boundary");
        app.update();
        // Polling forces actual GPU submissions to complete before checking the
        // target and proves this isn't a MinimalPlugins simulation-only capture.
        app.world()
            .resource::<RenderDevice>()
            .wgpu_device()
            .poll(PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(60)),
            })?;
        frames += 1;
        tracing::info!(target: "tracing_table::capture_boundary", frame = frames, phase = "end", "frame boundary");
        if frames % 30 == 0 {
            session.progress(frames, "rendered frames");
        }
        if frames >= settings.minimum_frames
            && session.capture.uncompressed_bytes() >= session.target
        {
            break;
        }
        if frames >= settings.max_frames {
            return Err("capture did not reach target before frame limit".into());
        }
    }
    Ok(frames)
}
