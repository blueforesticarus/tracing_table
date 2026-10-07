//! A standalone experiment: engine TRACE capture, scene instrumentation, and a
//! basic egui overlay. The root crate does not depend on this renderer package.
mod inspection;
mod model;
mod overlay;
mod scene;
mod strings;
use bevy::{log::LogPlugin, prelude::*};
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};
use inspection::InspectRecording;
use std::sync::{Arc, atomic::Ordering};
use tracing_table::subscriber::{RecordingLayer, USE_INDEXED_ATOM};

// Optional finite run for display/GPU smoke validation without a manual close.
#[derive(Resource)]
struct SmokeFrames(u32);
fn smoke_exit(mut frames: ResMut<SmokeFrames>, mut commands: Commands) {
    frames.0 = frames.0.saturating_sub(1);
    if frames.0 == 0 {
        commands.write_message(bevy::app::AppExit::Success);
    }
}

fn main() {
    // Configure string storage before installing tracing or starting Bevy workers.
    let indexed = std::env::var("USE_INDEXED_ATOM")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    USE_INDEXED_ATOM.store(indexed, Ordering::Relaxed);
    let recorder = Arc::new(RecordingLayer::new());
    // A global dispatch captures Bevy workers; no fmt layer or level filter is
    // added, so all tracing targets are recorded at TRACE without stderr spam.
    let control = inspection::RecordingControl::new(recorder.clone());
    tracing::subscriber::set_global_default(inspection::subscriber_with_control(control.clone()))
        .expect("install recording subscriber before Bevy startup");
    tracing_log::LogTracer::init().expect("bridge log crate events into tracing");
    let mut app = App::new();
    app.insert_resource(overlay::Viewer::new(recorder.clone(), control))
        .init_resource::<scene::SceneSettings>()
        .add_plugins(
            DefaultPlugins
                .build()
                .disable::<LogPlugin>()
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "tracing_table · Bevy compression viewer".into(),
                        resolution: (1440, 900).into(),
                        ..default()
                    }),
                    ..default()
                }),
        )
        .add_plugins(EguiPlugin::default())
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
        )
        .add_systems(EguiPrimaryContextPass, overlay::draw);
    let smoke = std::env::var("TRACING_VIEWER_SMOKE_FRAMES").ok();
    if let Some(frames) = &smoke {
        app.insert_resource(SmokeFrames(
            frames.parse().expect("positive smoke frame count"),
        ))
        .add_systems(Last, smoke_exit);
    }
    let exit = app.run();
    if smoke.is_some() {
        assert_eq!(exit, bevy::app::AppExit::Success);
        let snapshots = recorder.inspect(&Default::default());
        assert!(
            snapshots
                .iter()
                .any(|s| s.metadata.target().ends_with("::scene") && s.rows > 0)
        );
        println!(
            "Viewer smoke passed: {} archetypes, {:?}",
            snapshots.len(),
            recorder.compression_ratio()
        );
    }
}
