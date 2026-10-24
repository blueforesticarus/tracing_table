//! Extra reference workload and procedural scene assets are isolated from capture IO.
//! Tune probes/target size through the capture binary's environment settings.
use bevy::{
    animation::{AnimatedBy, AnimationTargetId, animated_field},
    asset::RenderAssetUsages,
    camera::RenderTarget,
    diagnostic::FrameCount,
    gltf::GltfAssetLabel,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
};

#[derive(Resource, Clone, Copy)]
pub struct CaptureSettings {
    pub probes: usize,
    pub width: u32,
    pub height: u32,
}
#[derive(Component)]
pub struct CaptureText;

pub fn setup(
    mut commands: Commands,
    cameras: Query<Entity, With<Camera3d>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut clips: ResMut<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    assets: Res<AssetServer>,
    settings: Res<CaptureSettings>,
) {
    // Offscreen output still runs camera extraction, render graphs, UI, and wgpu.
    let mut target = Image::new_fill(
        Extent3d {
            width: settings.width,
            height: settings.height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    target.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC;
    let target = images.add(target);
    let camera = cameras.single().expect("scene camera");
    commands
        .entity(camera)
        .insert(RenderTarget::Image(target.into()));
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(35.0, 35.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.15, 0.18, 0.22))),
        Transform::from_xyz(0.0, -3.1, 0.0),
    ));
    commands.spawn((
        PointLight {
            intensity: 500_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0),
    ));
    // Load a checked-in glTF through AssetServer, exercising IO, loaders, scenes,
    // mesh/material extraction and asynchronous asset events without network access.
    commands.spawn((
        WorldAssetRoot(assets.load(GltfAssetLabel::Scene(0).from_asset("capture_scene.gltf"))),
        Transform::from_xyz(-4.0, 0.0, 0.0).with_scale(Vec3::splat(2.0)),
    ));
    let name = Name::new("capture_animated_mesh");
    let id = AnimationTargetId::from_name(&name);
    let mut clip = AnimationClip::default();
    clip.add_curve_to_target(
        id,
        AnimatableCurve::new(
            animated_field!(Transform::translation),
            UnevenSampleAutoCurve::new([0.0, 1.0, 2.0].into_iter().zip([
                Vec3::new(4.0, 0.0, 0.0),
                Vec3::new(4.0, 2.0, 0.0),
                Vec3::new(4.0, 0.0, 0.0),
            ]))
            .unwrap(),
        ),
    );
    let (graph, index) = AnimationGraph::from_clip(clips.add(clip));
    let mut player = AnimationPlayer::default();
    player.play(index).repeat();
    let animated = commands
        .spawn((
            name,
            Mesh3d(meshes.add(Torus::default())),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.2, 0.8, 0.6),
                metallic: 0.7,
                ..default()
            })),
            AnimationGraphHandle(graphs.add(graph)),
            player,
        ))
        .id();
    commands.entity(animated).insert((id, AnimatedBy(animated)));
    // Changing text exercises layout, shaping, glyph atlases and UI rendering.
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(8.0),
                top: px(8.0),
                ..default()
            },
            UiTargetCamera(camera),
        ))
        .with_children(|parent| {
            parent.spawn((
                Text::new("reference capture"),
                TextFont {
                    font_size: FontSize::Px(16.0),
                    ..default()
                },
                CaptureText,
            ));
        });
}

pub fn update(
    mut gizmos: Gizmos,
    meshes: Query<(Entity, &Transform), With<Mesh3d>>,
    mut text: Query<&mut Text, With<CaptureText>>,
    settings: Res<CaptureSettings>,
    frames: Res<FrameCount>,
) {
    let frame = frames.0 as u64;
    let span = tracing::debug_span!("capture_frame", frame, records = tracing::field::Empty);
    let _entered = span.enter();
    let mut records = 0u64;
    for (entity, transform) in &meshes {
        gizmos.line(
            Vec3::ZERO,
            transform.translation,
            Color::srgb(0.2, 0.8, 1.0),
        );
        gizmos.sphere(
            Isometry3d::from_translation(transform.translation),
            0.25,
            Color::srgb(1.0, 0.4, 0.2),
        );
        for probe in 0..settings.probes {
            let entity = entity.to_bits();
            let sample = frame * settings.probes as u64 + probe as u64;
            let distance = transform.translation.length() as f64;
            tracing::trace!(
                entity,
                sample,
                x = transform.translation.x as f64,
                y = transform.translation.y as f64,
                z = transform.translation.z as f64,
                "entity {entity} transform sample {sample}"
            );
            tracing::trace!(entity, sample, rotation = ?transform.rotation, scale = ?transform.scale, message = "transform components");
            tracing::debug!(
                entity,
                sample,
                distance,
                visible = distance < 100.0,
                "entity {entity} bounds at {distance:.3}"
            );
            records += 3;
        }
    }
    if frame % 30 == 0 {
        for mut text in &mut text {
            **text = format!("Frame {frame} · scene + wgpu · é🦀");
        }
        tracing::info!(frame, records, message = "capture frame checkpoint");
    }
    span.record("records", records);
}
