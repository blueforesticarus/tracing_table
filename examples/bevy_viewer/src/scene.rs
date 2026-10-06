//! Adapted from Bevy's MIT/Apache-2.0 `examples/transforms/transform.rs`:
//! https://bevy.org/examples/transforms/transform/
//! Scene geometry and motion live here so the overlay can stay a plain viewer.
use bevy::prelude::*;
use std::f32::consts::PI;

#[derive(Resource)]
pub struct SceneSettings {
    pub logs_per_second: f32,
    pub move_speed: f32,
    pub turn_speed: f32,
    pub sphere_scale_factor: f32,
    elapsed: f32,
    pulse: bool,
    tick: u64,
}
impl Default for SceneSettings {
    fn default() -> Self {
        Self {
            logs_per_second: 30.0,
            move_speed: 2.0,
            turn_speed: 0.2,
            sphere_scale_factor: 0.05,
            elapsed: 0.0,
            pulse: false,
            tick: 0,
        }
    }
}

#[derive(Component)]
pub struct CubeState {
    start_pos: Vec3,
}
#[derive(Component)]
pub struct Center;

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Share mesh assets across cubes; only their transforms and material colors differ.
    let sphere = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(3.0).mesh().ico(4).unwrap())),
            MeshMaterial3d(materials.add(Color::srgb(1.0, 0.75, 0.05))),
            Transform::default(),
            Center,
        ))
        .id();
    tracing::info!(entity = sphere.to_bits(), message = "spawned center sphere");
    let cube_mesh = meshes.add(Cuboid::default());
    for (i, color) in [
        Color::WHITE,
        Color::srgb(0.2, 0.8, 1.0),
        Color::srgb(1.0, 0.3, 0.5),
    ]
    .into_iter()
    .enumerate()
    {
        let angle = i as f32 * 2.0 * PI / 3.0;
        let start_pos = Quat::from_rotation_y(angle) * (Vec3::Z * -10.0);
        let cube = commands
            .spawn((
                Mesh3d(cube_mesh.clone()),
                MeshMaterial3d(materials.add(color)),
                Transform::from_translation(start_pos)
                    .with_rotation(Quat::from_rotation_y(angle + PI / 2.0)),
                CubeState { start_pos },
            ))
            .id();
        tracing::info!(entity = cube.to_bits(), message = "spawned orbiting cube");
    }
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 15.0, 26.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((
        DirectionalLight::default(),
        Transform::from_xyz(3.0, 3.0, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

pub fn advance_log_clock(time: Res<Time>, mut settings: ResMut<SceneSettings>) {
    settings.elapsed += time.delta_secs();
    settings.pulse =
        settings.logs_per_second > 0.0 && settings.elapsed >= 1.0 / settings.logs_per_second;
    if settings.pulse {
        settings.elapsed %= 1.0 / settings.logs_per_second;
        settings.tick += 1;
    }
}

pub fn move_cube(
    mut cubes: Query<(Entity, &mut Transform), With<CubeState>>,
    time: Res<Time>,
    settings: Res<SceneSettings>,
) {
    // Logging remains synchronous on Bevy's system worker. Global subscriber
    // installation also captures systems running on other worker threads.
    let span = settings
        .pulse
        .then(|| tracing::trace_span!("move_cube_system", tick = settings.tick));
    let _entered = span.as_ref().map(|span| span.enter());
    for (entity, mut transform) in &mut cubes {
        let forward = transform.forward();
        transform.translation += forward * settings.move_speed * time.delta_secs();
        if settings.pulse {
            tracing::trace!(
                entity = entity.to_bits(),
                x = transform.translation.x as f64,
                z = transform.translation.z as f64,
                message = "moving cube"
            );
        }
    }
}

pub fn rotate_cube(
    mut cubes: Query<(Entity, &mut Transform), (With<CubeState>, Without<Center>)>,
    centers: Query<&Transform, With<Center>>,
    time: Res<Time>,
    settings: Res<SceneSettings>,
) {
    let center = centers
        .iter()
        .map(|transform| transform.translation)
        .sum::<Vec3>();
    for (entity, mut transform) in &mut cubes {
        let looking = transform.looking_at(center, *transform.local_y());
        transform.rotation = transform.rotation.lerp(
            looking.rotation,
            (settings.turn_speed * time.delta_secs()).min(1.0),
        );
        if settings.pulse && settings.tick % 4 == 0 {
            tracing::debug!(entity = entity.to_bits(), rotation = ?transform.rotation, message = "turning cube");
        }
    }
}

pub fn scale_center(
    cubes: Query<(&Transform, &CubeState), Without<Center>>,
    mut centers: Query<(Entity, &mut Transform), With<Center>>,
    settings: Res<SceneSettings>,
) {
    let distance = cubes
        .iter()
        .map(|(transform, cube)| (cube.start_pos - transform.translation).length())
        .sum::<f32>();
    for (entity, mut transform) in &mut centers {
        let scale = (1.0 - settings.sphere_scale_factor * distance / 3.0).max(0.1);
        transform.scale = Vec3::splat(scale);
        if settings.pulse && settings.tick % 16 == 0 {
            tracing::info!(
                entity = entity.to_bits(),
                scale = scale as f64,
                message = "scaling center sphere"
            );
        }
    }
}
