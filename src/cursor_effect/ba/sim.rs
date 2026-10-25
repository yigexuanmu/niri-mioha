//! BA 点击特效粒子模拟（Rust 移植）。
//!
//! 逐行移植自 `CialloKing/ba-click-fx-desktop` 的 `src/fx/src/simulation.cpp`
//! （`bafx::fx::Simulation`）。语义、随机数、时间戳推进顺序、回收时机都保持
//! 一致，唯一差别是时间用 `f64` 秒而不是 `chrono::nanoseconds`，以及不把
//! 作者色转进线性空间（见 `curve.rs` 顶部说明）。

use std::f32::consts::TAU;

use super::curve::{
    self, normalize_click_particle_settings, normalize_shard_particle_settings,
    normalize_time_scale, normalize_trail_length_multiplier, ClickParticleSettings, Color,
    ShardParticleSettings,
};

/// 世界空间里的一个点（y 向上、x 按屏幕宽度归一）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn add(self, other: Point) -> Point {
        Point::new(self.x + other.x, self.y + other.y)
    }

    fn sub(self, other: Point) -> Point {
        Point::new(self.x - other.x, self.y - other.y)
    }

    fn mul(self, scale: f32) -> Point {
        Point::new(self.x * scale, self.y * scale)
    }

    fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

fn direction(angle: f32) -> Point {
    Point::new(angle.cos(), angle.sin())
}

/// 视口尺寸（物理像素）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// 精灵材质种类。与上游 `SpriteKind` 的判别值一致（0/1/2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpriteKind {
    CenterDisk = 0,
    DissolveRing = 1,
    Triangle = 2,
}

/// 一帧里要绘制的一个精灵（屏幕像素坐标）。
#[derive(Debug, Clone, Copy)]
pub struct Sprite {
    pub kind: SpriteKind,
    pub center: Point,
    /// 精灵的四边长度（像素）。
    pub size: f32,
    pub rotation: f32,
    pub color: Color,
    /// Unity HDR 强度系数（线性域乘数）。
    pub intensity: f32,
    /// 溶解裁剪阈值（仅 `DissolveRing` 使用）。
    pub dissolve_threshold: f32,
    /// 三角图集的帧号（0 或 1）。
    pub atlas_frame: u32,
    pub render_queue: i32,
    pub contributes_bloom: bool,
    pub global_scale_pivot: Point,
    pub scale_center_with_global_scale: bool,
}

/// 拖尾顶点（屏幕像素 + 归一化年龄）。
#[derive(Debug, Clone, Copy)]
pub struct TrailPoint {
    pub position: Point,
    pub normalized_age: f32,
}

/// 一帧快照。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub sprites: Vec<Sprite>,
    pub trail: Vec<TrailPoint>,
    pub trail_width: f32,
    pub trail_opacity: f32,
    pub global_opacity: f32,
    pub active: bool,
    pub pointer_held: bool,
}

impl Snapshot {
    pub fn has_drawable_content(&self) -> bool {
        !self.sprites.is_empty() || (self.trail_width > 0.0 && self.trail.len() >= 2)
    }
}

// ─── 常量（与上游一致） ───

const TRIANGLE_LOCAL_SCALE: f32 = 0.307_882_4;
const DRAG_SHAPE_RADIUS_WORLD: f32 = 0.15 * TRIANGLE_LOCAL_SCALE;
const DRAG_EMISSION_STEP_WORLD: f32 = 1.0 / 5.0;
const TRAIL_POINT_STEP_WORLD: f32 = 0.01;
const TRAIL_LIFETIME_SECONDS: f64 = 0.3;
const TRAIL_PARKING_TIME_SCALE_THRESHOLD: f32 = 0.19;
const TRAIL_WIDTH_WORLD: f32 = 0.005;
const REFERENCE_WORLD_TO_PIXELS: f32 = 540.0;
/// 重建的 Cylinder002 网格外半径（见 `ring_mesh`）。
pub const WEB_RING_MESH_OUTER_RADIUS: f32 = 1.063_668_4;
const MAXIMUM_PARTICLE_TIMESTEP_SECONDS: f32 = 0.03;
const MAXIMUM_DRAG_PARTICLES: u32 = 50;
const RELEASE_LIFETIME_SECONDS: f64 = 1.0;
/// 零延迟 burst 最多消耗一个不带年龄的粒子步，再加到最长寿命上。
const MAXIMUM_RELEASE_RETENTION_SECONDS: f64 = (10_000.0 * 0.001 + 0.03) / 0.01;
const ATLAS_RANDOM_STREAM: u64 = 0xD1B5_4A32_D192_ED03;

/// 激活时的默认随机种子（与上游 `Simulation` 构造函数默认参数一致）。
pub const DEFAULT_SEED: u64 = 20_260_716;

fn ms_to_seconds(milliseconds: f32) -> f32 {
    milliseconds * 0.001
}

fn reference_radius_to_start_size_world(radius_pixels: f32) -> f32 {
    radius_pixels / (REFERENCE_WORLD_TO_PIXELS * WEB_RING_MESH_OUTER_RADIUS)
}

/// SplitMix64，确定性、无平台库漂移。
struct Random {
    state: u64,
}

impl Random {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    fn unit(&mut self) -> f32 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^= value >> 31;
        let high = (value >> 40) as u32;
        high as f32 / (1u32 << 24) as f32
    }

    fn range(&mut self, minimum: f32, maximum: f32) -> f32 {
        minimum + (maximum - minimum) * self.unit()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct MovingParticle {
    origin_world: Point,
    velocity_world: Point,
    born_at: f64,
    lifetime_seconds: f32,
    start_size_world: f32,
    atlas_frame: u32,
    drag_particle: bool,
    global_scale_pivot_world: Point,
}

#[derive(Debug, Clone, Copy, Default)]
struct RingParticle {
    start_size_world: f32,
    initial_rotation_radians: f32,
    angular_blend: f32,
    settled_rotation_radians: f32,
    rotation_anchor_age_seconds: f32,
}

#[derive(Debug, Clone, Copy, Default)]
struct StoredTrailPoint {
    world: Point,
    created_at: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ParticleStepState {
    particle_age_seconds: f32,
    custom_data_age_seconds: f32,
    burst_emitted: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct ClickParticleStepStates {
    center_disk: ParticleStepState,
    dissolve_rings: ParticleStepState,
    click_triangles: ParticleStepState,
}

/// 图层可见性开关（对应上游 `EffectLayerVisibility`，默认全开）。
#[derive(Debug, Clone, Copy)]
pub struct LayerVisibility {
    pub center_disk: bool,
    pub dissolve_rings: bool,
    pub click_shards: bool,
    pub trail_shards: bool,
    pub trail: bool,
}

impl Default for LayerVisibility {
    fn default() -> Self {
        Self {
            center_disk: true,
            dissolve_rings: true,
            click_shards: true,
            trail_shards: true,
            trail: true,
        }
    }
}

/// BA 点击特效的一次模拟实例。
pub struct Simulation {
    base_seed: u64,
    activation_count: u64,
    random: Random,
    atlas_random: Random,
    active: bool,
    pointer_held: bool,
    click_effect_enabled: bool,
    first_advance_pending: bool,
    last_advanced_at: f64,
    released_at: f64,
    click_time_source_at: f64,
    pending_click_time: f64,
    trail_time_source_at: f64,
    trail_time: f64,
    pointer_trail_sample_at: f64,
    effect_origin_world: Point,
    pointer_world: Point,
    pointer_sample_at: f64,
    last_emission_world: Point,
    drag_distance_remainder_world: f32,
    trail_length_multiplier: f32,
    layer_visibility: LayerVisibility,
    click_time_scale: f32,
    trail_time_scale: f32,
    click_particle_settings: ClickParticleSettings,
    shard_particle_settings: ShardParticleSettings,
    trail_parking_mode: bool,
    trail_renderer_enabled: bool,
    particle_step_states: ClickParticleStepStates,
    rings: Vec<RingParticle>,
    triangles: Vec<MovingParticle>,
    trail: Vec<StoredTrailPoint>,
    trail_parking_points: Vec<StoredTrailPoint>,
}

impl Default for Simulation {
    fn default() -> Self {
        Self::new(DEFAULT_SEED)
    }
}

impl Simulation {
    pub fn new(seed: u64) -> Self {
        Self {
            base_seed: seed,
            activation_count: 0,
            random: Random::new(seed),
            atlas_random: Random::new(seed ^ ATLAS_RANDOM_STREAM),
            active: false,
            pointer_held: false,
            click_effect_enabled: false,
            first_advance_pending: false,
            last_advanced_at: 0.0,
            released_at: 0.0,
            click_time_source_at: 0.0,
            pending_click_time: 0.0,
            trail_time_source_at: 0.0,
            trail_time: 0.0,
            pointer_trail_sample_at: 0.0,
            effect_origin_world: Point::default(),
            pointer_world: Point::default(),
            pointer_sample_at: 0.0,
            last_emission_world: Point::default(),
            drag_distance_remainder_world: 0.0,
            trail_length_multiplier: 1.0,
            layer_visibility: LayerVisibility::default(),
            click_time_scale: 1.0,
            trail_time_scale: 1.0,
            click_particle_settings: ClickParticleSettings::default(),
            shard_particle_settings: ShardParticleSettings::default(),
            trail_parking_mode: false,
            trail_renderer_enabled: true,
            particle_step_states: ClickParticleStepStates::default(),
            rings: Vec::with_capacity(64),
            triangles: Vec::with_capacity(1050),
            trail: Vec::with_capacity(128),
            trail_parking_points: Vec::with_capacity(128),
        }
    }

    pub fn set_trail_length_multiplier(&mut self, multiplier: f32) {
        self.trail_length_multiplier = normalize_trail_length_multiplier(multiplier);
        if self.trail_length_multiplier == 0.0 {
            self.trail.clear();
            self.trail_parking_points.clear();
        }
    }

    pub fn set_layer_visibility(&mut self, visibility: LayerVisibility) {
        self.layer_visibility = visibility;
    }

    /// 只在非激活状态下生效的点击时间尺度（带时间戳的重载才安全切分历史）。
    pub fn set_click_time_scale(&mut self, time_scale: f32, time: f64) {
        self.accumulate_click_time(time);
        self.click_time_scale = normalize_time_scale(time_scale);
    }

    pub fn set_trail_time_scale(&mut self, time_scale: f32, time: f64) {
        self.synchronize_trail_time(time);
        self.trail_time_scale = normalize_time_scale(time_scale);
    }

    pub fn set_click_particle_settings(&mut self, settings: ClickParticleSettings) {
        self.click_particle_settings = normalize_click_particle_settings(settings);
    }

    pub fn set_click_particle_settings_at(
        &mut self,
        settings: ClickParticleSettings,
        time: f64,
    ) {
        let normalized = normalize_click_particle_settings(settings);
        let motion_changed = normalized.rings_lifetime_ms
            != self.click_particle_settings.rings_lifetime_ms
            || normalized.rings_angular_velocity_multiplier
                != self.click_particle_settings.rings_angular_velocity_multiplier
            || normalized.rings_rotation_direction
                != self.click_particle_settings.rings_rotation_direction;
        if motion_changed && self.active && self.click_effect_enabled && !self.rings.is_empty() {
            self.accumulate_click_time(time);
            let age = self.particle_step_states_at(time).dissolve_rings.particle_age_seconds;
            self.settle_ring_rotation(age);
        }
        self.click_particle_settings = normalized;
    }

    pub fn set_shard_particle_settings(&mut self, settings: ShardParticleSettings) {
        self.shard_particle_settings = normalize_shard_particle_settings(settings);
    }

    /// 与 `FxTrailTimeScale.Update` 等价：低时间尺度进入「停放」收缩序列。
    pub fn update_unity_trail_time_scale(&mut self, time_scale: f32) {
        let normalized = if time_scale.is_finite() {
            time_scale.clamp(0.0, 4.0)
        } else {
            time_scale
        };
        if normalized > TRAIL_PARKING_TIME_SCALE_THRESHOLD {
            if self.trail_parking_mode {
                self.trail_parking_mode = false;
                self.init_trail_normal_mode();
            }
            return;
        }
        if !self.trail_parking_mode {
            self.trail_parking_mode = true;
            self.init_trail_parking_mode();
            return;
        }
        if self.trail_renderer_enabled {
            self.step_trail_parking_sequence();
        }
    }

    pub fn pointer_down(&mut self, screen_position: Point, viewport: Viewport, time: f64) {
        let world = Self::screen_to_world(screen_position, viewport);
        self.reset(world, time);
    }

    /// 只起拖动实例，不生成点击爆裂（常驻拖尾用）。
    pub fn start_trail(&mut self, screen_position: Point, viewport: Viewport, time: f64) {
        let world = Self::screen_to_world(screen_position, viewport);
        self.reset_state(world, time);
        self.append_trail_point(world, self.trail_time);
    }

    pub fn pointer_move(&mut self, screen_position: Point, viewport: Viewport, time: f64) {
        if !self.active || !self.pointer_held {
            return;
        }
        let sample_time = time.max(self.pointer_sample_at);
        self.synchronize_trail_time(sample_time);
        let sample_trail_time = self.trail_time;
        let next_world = Self::screen_to_world(screen_position, viewport);
        if self.click_effect_enabled && self.first_advance_pending {
            // Unity 首次粒子步之前只有最终 transform，没有走过的线段。
            self.relocate_pending_click(next_world, sample_time, sample_trail_time);
            return;
        }

        self.emit_along_drag(
            self.pointer_world,
            next_world,
            self.pointer_trail_sample_at,
            sample_trail_time,
        );
        self.pointer_world = next_world;
        self.pointer_sample_at = sample_time;
        self.pointer_trail_sample_at = sample_trail_time;
        self.append_trail_point(self.pointer_world, sample_trail_time);
    }

    pub fn pointer_up(&mut self, time: f64) {
        if !self.active || !self.pointer_held {
            return;
        }
        self.pointer_held = false;
        self.pointer_sample_at = self.pointer_sample_at.max(time);
        self.synchronize_trail_time(self.pointer_sample_at);
        self.pointer_trail_sample_at = self.trail_time;
        self.released_at = self.pointer_sample_at;
    }

    pub fn pointer_cancel(&mut self, time: f64) {
        if !self.active || !self.pointer_held {
            return;
        }
        self.pointer_held = false;
        self.pointer_sample_at = self.pointer_sample_at.max(time);
        self.synchronize_trail_time(self.pointer_sample_at);
        self.pointer_trail_sample_at = self.trail_time;
        self.released_at = self.pointer_sample_at;
        self.drag_distance_remainder_world = 0.0;
    }

    pub fn advance(&mut self, time: f64) {
        if !self.active || time < self.last_advanced_at {
            return;
        }

        if self.pointer_held {
            self.pointer_sample_at = self.pointer_sample_at.max(time);
        }
        self.first_advance_pending = false;
        self.accumulate_click_time(time);
        self.synchronize_trail_time(time);
        if self.pointer_held {
            self.pointer_trail_sample_at = self.trail_time;
        }
        if self.click_effect_enabled {
            let pending = self.pending_click_time;
            advance_click_particle_step_states(&mut self.particle_step_states, pending);
        }
        self.pending_click_time = 0.0;

        self.last_advanced_at = time;
        let effective_trail_lifetime = TRAIL_LIFETIME_SECONDS * self.trail_length_multiplier as f64;
        if !self.trail_parking_mode {
            let trail_time = self.trail_time;
            self.trail.retain(|point| {
                effective_trail_lifetime > 0.0
                    && (trail_time - point.created_at) < effective_trail_lifetime
            });
        }

        let click_triangle_age = self.particle_step_states.click_triangles.particle_age_seconds;
        let click_triangles_emitted = self.particle_step_states.click_triangles.burst_emitted;
        let trail_time = self.trail_time;
        self.triangles.retain(|particle| {
            if particle.drag_particle {
                return (trail_time - particle.born_at) <= particle.lifetime_seconds as f64;
            }
            !(click_triangles_emitted && click_triangle_age > particle.lifetime_seconds)
        });
    }

    pub fn on_frame_rendered(&mut self, time: f64) {
        if !self.active || self.pointer_held {
            return;
        }

        let released_age = time - self.released_at;
        if released_age < RELEASE_LIFETIME_SECONDS {
            return;
        }

        if self.trail_parking_mode {
            self.trail.clear();
        }

        if released_age < MAXIMUM_RELEASE_RETENTION_SECONDS
            && self.has_visible_systems_after_frame(time)
        {
            return;
        }

        self.active = false;
        self.rings.clear();
        self.triangles.clear();
        self.trail.clear();
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn pointer_held(&self) -> bool {
        self.pointer_held
    }

    pub fn first_advance_pending(&self) -> bool {
        self.first_advance_pending
    }

    fn has_visible_systems_after_frame(&self, time: f64) -> bool {
        let states = self.particle_step_states_at(time);
        if self.click_effect_enabled {
            let disk_visible = self.layer_visibility.center_disk
                && (!states.center_disk.burst_emitted
                    || states.center_disk.particle_age_seconds
                        < ms_to_seconds(self.click_particle_settings.disk_lifetime_ms));
            if disk_visible {
                return true;
            }
            let rings_visible = self.layer_visibility.dissolve_rings
                && !self.rings.is_empty()
                && (!states.dissolve_rings.burst_emitted
                    || states.dissolve_rings.particle_age_seconds
                        < ms_to_seconds(self.click_particle_settings.rings_lifetime_ms));
            if rings_visible {
                return true;
            }
        }

        let current_trail_time = self.trail_time_at(time);
        for particle in &self.triangles {
            if particle.drag_particle {
                if self.layer_visibility.trail_shards
                    && (current_trail_time - particle.born_at) < particle.lifetime_seconds as f64
                {
                    return true;
                }
                continue;
            }
            if self.layer_visibility.click_shards
                && self.click_effect_enabled
                && (!states.click_triangles.burst_emitted
                    || states.click_triangles.particle_age_seconds < particle.lifetime_seconds)
            {
                return true;
            }
        }

        if !self.layer_visibility.trail
            || !self.trail_renderer_enabled
            || self.trail_length_multiplier <= 0.0
            || self.trail.len() < 2
        {
            return false;
        }
        if self.trail_parking_mode {
            return false;
        }

        let effective_trail_lifetime = TRAIL_LIFETIME_SECONDS * self.trail_length_multiplier as f64;
        let mut first_visible: Option<&StoredTrailPoint> = None;
        for point in &self.trail {
            if (current_trail_time - point.created_at) >= effective_trail_lifetime {
                continue;
            }
            match first_visible {
                None => first_visible = Some(point),
                Some(first) => {
                    if point.world.x != first.world.x || point.world.y != first.world.y {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn snapshot(&self, viewport: Viewport, time: f64) -> Snapshot {
        let mut frame = Snapshot {
            active: self.active,
            pointer_held: self.pointer_held,
            trail_width: TRAIL_WIDTH_WORLD * Self::world_to_pixels(viewport),
            trail_opacity: 1.0,
            global_opacity: 1.0,
            ..Default::default()
        };
        if !self.active || viewport.width == 0 || viewport.height == 0 {
            return frame;
        }

        let states = self.particle_step_states_at(time);
        let snapshot_trail_time = self.trail_time_at(time);

        let disk_lifetime = ms_to_seconds(self.click_particle_settings.disk_lifetime_ms);
        if self.layer_visibility.center_disk
            && self.click_effect_enabled
            && states.center_disk.burst_emitted
            && states.center_disk.particle_age_seconds <= disk_lifetime
        {
            let normalized_age = states.center_disk.particle_age_seconds / disk_lifetime;
            frame.sprites.push(Sprite {
                kind: SpriteKind::CenterDisk,
                center: Self::world_to_screen(self.effect_origin_world, viewport),
                size: 0.12
                    * curve::disk_size_curve(normalized_age)
                    * Self::world_to_pixels(viewport),
                rotation: 0.0,
                color: curve::disk_color(normalized_age),
                intensity: 2.0,
                dissolve_threshold: 0.0,
                atlas_frame: 0,
                render_queue: 4499,
                contributes_bloom: true,
                global_scale_pivot: Point::default(),
                scale_center_with_global_scale: false,
            });
        }

        let ring_lifetime = ms_to_seconds(self.click_particle_settings.rings_lifetime_ms);
        if self.layer_visibility.dissolve_rings
            && states.dissolve_rings.burst_emitted
            && states.dissolve_rings.particle_age_seconds <= ring_lifetime
        {
            let normalized_age = states.dissolve_rings.particle_age_seconds / ring_lifetime;
            let custom_normalized_age =
                states.dissolve_rings.custom_data_age_seconds / ring_lifetime;
            for ring in &self.rings {
                let size = ring.start_size_world
                    * curve::ring_size_curve(normalized_age)
                    * 2.127_337
                    * Self::world_to_pixels(viewport);
                frame.sprites.push(Sprite {
                    kind: SpriteKind::DissolveRing,
                    center: Self::world_to_screen(self.effect_origin_world, viewport),
                    size,
                    rotation: ring.initial_rotation_radians
                        + ring.settled_rotation_radians
                        + curve::ring_rotation_delta(
                            ring.angular_blend,
                            ring.rotation_anchor_age_seconds,
                            states.dissolve_rings.particle_age_seconds,
                            &self.click_particle_settings,
                        ),
                    color: curve::ring_color(normalized_age),
                    intensity: 5.992_157,
                    dissolve_threshold: curve::dissolve_threshold(custom_normalized_age),
                    atlas_frame: 0,
                    render_queue: 4499,
                    contributes_bloom: true,
                    global_scale_pivot: Point::default(),
                    scale_center_with_global_scale: false,
                });
            }
        }

        for particle in &self.triangles {
            if (particle.drag_particle && !self.layer_visibility.trail_shards)
                || (!particle.drag_particle && !self.layer_visibility.click_shards)
            {
                continue;
            }

            let age: f64 = if particle.drag_particle {
                let age = snapshot_trail_time - particle.born_at;
                if age <= 0.0 {
                    continue;
                }
                age
            } else {
                let state = &states.click_triangles;
                if !state.burst_emitted {
                    continue;
                }
                state.particle_age_seconds as f64
            };

            if age > particle.lifetime_seconds as f64 {
                continue;
            }

            let normalized_age = (age / particle.lifetime_seconds as f64) as f32;
            let world_position = particle
                .origin_world
                .add(particle.velocity_world.mul(age as f32));
            frame.sprites.push(Sprite {
                kind: SpriteKind::Triangle,
                center: Self::world_to_screen(world_position, viewport),
                size: particle.start_size_world
                    * curve::triangle_size_curve(normalized_age)
                    * Self::world_to_pixels(viewport),
                rotation: 0.0,
                color: curve::triangle_color(normalized_age),
                intensity: 5.992_157,
                dissolve_threshold: 0.0,
                atlas_frame: particle.atlas_frame,
                render_queue: 4550,
                contributes_bloom: true,
                global_scale_pivot: Self::world_to_screen(
                    particle.global_scale_pivot_world,
                    viewport,
                ),
                scale_center_with_global_scale: true,
            });
        }

        let effective_trail_lifetime = TRAIL_LIFETIME_SECONDS * self.trail_length_multiplier as f64;
        if self.layer_visibility.trail
            && self.trail_renderer_enabled
            && effective_trail_lifetime > 0.0
        {
            for point in &self.trail {
                let normalized_age = (((snapshot_trail_time - point.created_at)
                    / effective_trail_lifetime) as f32)
                    .clamp(0.0, 1.0);
                frame.trail.push(TrailPoint {
                    position: Self::world_to_screen(point.world, viewport),
                    normalized_age,
                });
            }
        }

        frame
            .sprites
            .sort_by_key(|sprite| sprite.render_queue);
        frame
    }

    pub fn has_drawable_content(&self, time: f64) -> bool {
        if !self.active {
            return false;
        }

        let states = self.particle_step_states_at(time);
        if self.layer_visibility.center_disk
            && self.click_effect_enabled
            && states.center_disk.burst_emitted
            && states.center_disk.particle_age_seconds
                <= ms_to_seconds(self.click_particle_settings.disk_lifetime_ms)
        {
            return true;
        }
        if self.layer_visibility.dissolve_rings
            && !self.rings.is_empty()
            && states.dissolve_rings.burst_emitted
            && states.dissolve_rings.particle_age_seconds
                <= ms_to_seconds(self.click_particle_settings.rings_lifetime_ms)
        {
            return true;
        }
        let current_trail_time = self.trail_time_at(time);
        for particle in &self.triangles {
            if particle.drag_particle {
                let age = current_trail_time - particle.born_at;
                if self.layer_visibility.trail_shards
                    && age > 0.0
                    && age <= particle.lifetime_seconds as f64
                {
                    return true;
                }
                continue;
            }
            if self.layer_visibility.click_shards
                && states.click_triangles.burst_emitted
                && states.click_triangles.particle_age_seconds <= particle.lifetime_seconds
            {
                return true;
            }
        }
        self.layer_visibility.trail && self.trail_renderer_enabled && !self.trail.is_empty()
    }

    // ─── 内部推进 ───

    fn settle_ring_rotation(&mut self, particle_age_seconds: f32) {
        let settings = self.click_particle_settings;
        for ring in &mut self.rings {
            ring.settled_rotation_radians += curve::ring_rotation_delta(
                ring.angular_blend,
                ring.rotation_anchor_age_seconds,
                particle_age_seconds,
                &settings,
            );
            ring.rotation_anchor_age_seconds = particle_age_seconds;
        }
    }

    fn screen_to_world(screen: Point, viewport: Viewport) -> Point {
        if viewport.width == 0 || viewport.height == 0 {
            return Point::default();
        }
        let width = viewport.width as f32;
        let height = viewport.height as f32;
        let aspect = width / height;
        Point::new(
            (screen.x / width * 2.0 - 1.0) * aspect,
            1.0 - screen.y / height * 2.0,
        )
    }

    fn world_to_screen(world: Point, viewport: Viewport) -> Point {
        if viewport.width == 0 || viewport.height == 0 {
            return Point::default();
        }
        let width = viewport.width as f32;
        let height = viewport.height as f32;
        let aspect = width / height;
        Point::new(
            (world.x / aspect + 1.0) * 0.5 * width,
            (1.0 - world.y) * 0.5 * height,
        )
    }

    fn world_to_pixels(viewport: Viewport) -> f32 {
        viewport.height as f32 * 0.5
    }

    fn scaled_duration(duration: f64, time_scale: f32) -> f64 {
        if duration <= 0.0 {
            return 0.0;
        }
        duration * time_scale as f64
    }

    fn accumulate_click_time(&mut self, time: f64) {
        if time <= self.click_time_source_at {
            return;
        }
        self.pending_click_time +=
            Self::scaled_duration(time - self.click_time_source_at, self.click_time_scale);
        self.click_time_source_at = time;
    }

    fn synchronize_trail_time(&mut self, time: f64) {
        if time <= self.trail_time_source_at {
            return;
        }
        self.trail_time +=
            Self::scaled_duration(time - self.trail_time_source_at, self.trail_time_scale);
        self.trail_time_source_at = time;
    }

    fn trail_time_at(&self, time: f64) -> f64 {
        if time <= self.trail_time_source_at {
            return self.trail_time;
        }
        self.trail_time
            + Self::scaled_duration(time - self.trail_time_source_at, self.trail_time_scale)
    }

    fn particle_step_states_at(&self, time: f64) -> ClickParticleStepStates {
        let mut states = self.particle_step_states;
        let mut elapsed = self.pending_click_time;
        if time > self.click_time_source_at {
            elapsed += Self::scaled_duration(
                time - self.click_time_source_at,
                self.click_time_scale,
            );
        }
        if self.click_effect_enabled && elapsed > 0.0 {
            advance_click_particle_step_states(&mut states, elapsed);
        }
        states
    }

    fn reset(&mut self, world_position: Point, time: f64) {
        self.reset_state(world_position, time);
        self.click_effect_enabled = true;

        let settings = self.click_particle_settings;
        let radius_minimum = reference_radius_to_start_size_world(settings.rings_radius_min);
        let radius_maximum = reference_radius_to_start_size_world(settings.rings_radius_max);
        for _ in 0..settings.rings_count {
            self.rings.push(RingParticle {
                start_size_world: self.random.range(radius_minimum, radius_maximum),
                initial_rotation_radians: self.random.range(0.0, TAU),
                angular_blend: self.random.unit(),
                settled_rotation_radians: 0.0,
                rotation_anchor_age_seconds: 0.0,
            });
        }
        self.emit_click_triangles(0.0);
        self.append_trail_point(world_position, self.trail_time);
    }

    fn reset_state(&mut self, world_position: Point, time: f64) {
        self.activation_count += 1;
        let activation_seed = self
            .base_seed
            .wrapping_add(self.activation_count.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        self.random = Random::new(activation_seed);
        self.atlas_random = Random::new(activation_seed ^ ATLAS_RANDOM_STREAM);
        self.active = true;
        self.pointer_held = true;
        self.click_effect_enabled = false;
        self.first_advance_pending = true;
        self.last_advanced_at = time;
        self.released_at = time;
        self.click_time_source_at = time;
        self.pending_click_time = 0.0;
        self.trail_time_source_at = time;
        self.pointer_trail_sample_at = self.trail_time;
        self.effect_origin_world = world_position;
        self.pointer_world = world_position;
        self.pointer_sample_at = time;
        self.last_emission_world = world_position;
        self.drag_distance_remainder_world = 0.0;
        self.particle_step_states = ClickParticleStepStates::default();
        self.rings.clear();
        self.triangles.clear();
        self.trail.clear();
    }

    fn relocate_pending_click(&mut self, world_position: Point, time: f64, trail_time: f64) {
        let offset = world_position.sub(self.effect_origin_world);
        self.effect_origin_world = world_position;
        self.pointer_world = world_position;
        self.pointer_sample_at = time;
        self.pointer_trail_sample_at = trail_time;
        self.last_emission_world = world_position;
        self.drag_distance_remainder_world = 0.0;

        for particle in &mut self.triangles {
            if !particle.drag_particle {
                particle.origin_world = particle.origin_world.add(offset);
                particle.global_scale_pivot_world =
                    particle.global_scale_pivot_world.add(offset);
            }
        }

        if !self.trail.is_empty() {
            self.trail[0] = StoredTrailPoint {
                world: world_position,
                created_at: trail_time,
            };
            self.trail.truncate(1);
        }
    }

    fn emit_click_triangles(&mut self, click_time: f64) {
        let settings = self.shard_particle_settings;
        let radius_world = settings.click_radius / REFERENCE_WORLD_TO_PIXELS;
        let speed_minimum_world = settings.click_speed_min / REFERENCE_WORLD_TO_PIXELS;
        let speed_maximum_world = settings.click_speed_max / REFERENCE_WORLD_TO_PIXELS;
        let size_minimum_world = settings.size_min / REFERENCE_WORLD_TO_PIXELS;
        let size_maximum_world = settings.size_max / REFERENCE_WORLD_TO_PIXELS;
        for _ in 0..settings.click_count {
            let angle = self.random.range(0.0, TAU);
            let radial = direction(angle);
            let origin = self.effect_origin_world;
            let velocity = radial.mul(self.random.range(speed_minimum_world, speed_maximum_world));
            let lifetime = ms_to_seconds(
                self.random
                    .range(settings.click_lifetime_min_ms, settings.click_lifetime_max_ms),
            );
            let size = self.random.range(size_minimum_world, size_maximum_world);
            let atlas_frame = if self.atlas_random.unit() < 0.5 { 0 } else { 1 };
            self.triangles.push(MovingParticle {
                origin_world: origin.add(radial.mul(radius_world)),
                velocity_world: velocity,
                born_at: click_time,
                lifetime_seconds: lifetime,
                start_size_world: size,
                atlas_frame,
                drag_particle: false,
                global_scale_pivot_world: origin,
            });
        }
    }

    fn emit_drag_triangle(&mut self, world_position: Point, trail_time: f64) {
        let drag_count = self
            .triangles
            .iter()
            .filter(|particle| particle.drag_particle)
            .count() as u32;
        if drag_count >= MAXIMUM_DRAG_PARTICLES {
            return;
        }

        let settings = self.shard_particle_settings;
        let angle = self.random.range(0.0, TAU);
        let radial = direction(angle);
        let size_minimum_world = settings.size_min / REFERENCE_WORLD_TO_PIXELS;
        let size_maximum_world = settings.size_max / REFERENCE_WORLD_TO_PIXELS;
        let velocity = radial.mul(self.random.range(0.2, 0.3) * TRIANGLE_LOCAL_SCALE);
        let lifetime = self.random.range(0.2, 0.4);
        let size = self.random.range(size_minimum_world, size_maximum_world);
        let atlas_frame = if self.atlas_random.unit() < 0.5 { 0 } else { 1 };
        self.triangles.push(MovingParticle {
            origin_world: world_position.add(radial.mul(DRAG_SHAPE_RADIUS_WORLD)),
            velocity_world: velocity,
            born_at: trail_time,
            lifetime_seconds: lifetime,
            start_size_world: size,
            atlas_frame,
            drag_particle: true,
            global_scale_pivot_world: world_position,
        });
    }

    fn append_trail_point(&mut self, world_position: Point, trail_time: f64) {
        if self.trail_length_multiplier <= 0.0
            || self.trail_parking_mode
            || !self.trail_renderer_enabled
        {
            return;
        }

        if self.trail.is_empty() {
            self.trail.push(StoredTrailPoint {
                world: world_position,
                created_at: trail_time,
            });
            return;
        }

        let effective_trail_lifetime = TRAIL_LIFETIME_SECONDS * self.trail_length_multiplier as f64;
        let last = self.trail[self.trail.len() - 1];
        if (trail_time - last.created_at) >= effective_trail_lifetime {
            self.trail.clear();
            self.trail.push(StoredTrailPoint {
                world: world_position,
                created_at: trail_time,
            });
            return;
        }

        if world_position.sub(last.world).length() < TRAIL_POINT_STEP_WORLD {
            return;
        }

        self.trail.push(StoredTrailPoint {
            world: world_position,
            created_at: trail_time,
        });
    }

    fn init_trail_normal_mode(&mut self) {
        self.trail_parking_points.clear();
        self.trail_renderer_enabled = true;
    }

    fn init_trail_parking_mode(&mut self) {
        self.trail_parking_points.clear();
        if self.trail.is_empty() {
            self.finish_trail_parking_sequence();
            return;
        }
        if self.trail.len() == 1 {
            return;
        }
        self.trail_parking_points
            .extend_from_slice(&self.trail[1..]);
    }

    fn step_trail_parking_sequence(&mut self) {
        if self.trail_parking_points.len() <= 1 {
            self.finish_trail_parking_sequence();
            return;
        }
        self.trail = self.trail_parking_points.clone();
        self.trail_parking_points.remove(0);
    }

    fn finish_trail_parking_sequence(&mut self) {
        self.trail_parking_points.clear();
        self.trail.clear();
        self.trail_renderer_enabled = false;
    }

    fn emit_along_drag(
        &mut self,
        from: Point,
        to: Point,
        from_trail_time: f64,
        to_trail_time: f64,
    ) {
        let segment = to.sub(from);
        let segment_length = segment.length();
        if segment_length <= f32::EPSILON {
            return;
        }

        let mut consumed = 0.0f32;
        let mut distance_until_emission =
            DRAG_EMISSION_STEP_WORLD - self.drag_distance_remainder_world;
        while consumed + distance_until_emission <= segment_length {
            consumed += distance_until_emission;
            let interpolation = consumed / segment_length;
            let position = from.add(segment.mul(interpolation));
            let trail_time = from_trail_time
                + (to_trail_time - from_trail_time) * interpolation as f64;
            self.emit_drag_triangle(position, trail_time);
            self.last_emission_world = position;
            self.drag_distance_remainder_world = 0.0;
            distance_until_emission = DRAG_EMISSION_STEP_WORLD;
        }
        self.drag_distance_remainder_world += segment_length - consumed;
    }
}

fn advance_particle_step_state(state: &mut ParticleStepState, elapsed: f64) {
    let elapsed_seconds = elapsed as f32;
    if elapsed_seconds <= 0.0 {
        return;
    }
    let step_count = (elapsed_seconds / MAXIMUM_PARTICLE_TIMESTEP_SECONDS).ceil() as u32;
    let step_seconds = elapsed_seconds / step_count as f32;
    for _ in 0..step_count {
        if !state.burst_emitted {
            state.burst_emitted = true;
            continue;
        }
        state.custom_data_age_seconds = state.particle_age_seconds;
        state.particle_age_seconds += step_seconds;
    }
}

fn advance_click_particle_step_states(states: &mut ClickParticleStepStates, elapsed: f64) {
    advance_particle_step_state(&mut states.center_disk, elapsed);
    advance_particle_step_state(&mut states.dissolve_rings, elapsed);
    advance_particle_step_state(&mut states.click_triangles, elapsed);
}

/// 对快照应用全局缩放（尺寸与拖着走的碎片绕各自发射点缩放）。
pub fn apply_global_scale(snapshot: &mut Snapshot, scale: f32) {
    let safe_scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    for sprite in &mut snapshot.sprites {
        sprite.size *= safe_scale;
        if sprite.scale_center_with_global_scale {
            let offset = sprite.center.sub(sprite.global_scale_pivot);
            sprite.center = sprite
                .global_scale_pivot
                .add(offset.mul(safe_scale));
        }
    }
    snapshot.trail_width *= safe_scale;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_burst_spawns_disk_rings_and_shards() {
        let viewport = Viewport::new(1920, 1080);
        let mut sim = Simulation::default();
        sim.pointer_down(Point::new(960.0, 540.0), viewport, 0.0);
        // 第一帧只有 burst，没有年龄。
        let snapshot = sim.snapshot(viewport, 0.016);
        assert!(snapshot.active);
        // 圆盘 + 2 个环 + 4 个碎片
        assert_eq!(snapshot.sprites.len(), 7, "{snapshot:?}");
        assert_eq!(snapshot.sprites[0].kind, SpriteKind::CenterDisk);
        assert_eq!(
            snapshot
                .sprites
                .iter()
                .filter(|s| s.kind == SpriteKind::DissolveRing)
                .count(),
            2
        );
        assert_eq!(
            snapshot
                .sprites
                .iter()
                .filter(|s| s.kind == SpriteKind::Triangle)
                .count(),
            4
        );
    }

    #[test]
    fn trail_survives_until_release_window() {
        let viewport = Viewport::new(1920, 1080);
        let mut sim = Simulation::default();
        sim.pointer_down(Point::new(100.0, 100.0), viewport, 0.0);
        sim.advance(0.016);
        let mut t = 0.016;
        for step in 0..10 {
            t += 0.016;
            sim.pointer_move(Point::new(100.0 + step as f32 * 12.0, 120.0), viewport, t);
            sim.advance(t);
        }
        let snapshot = sim.snapshot(viewport, t);
        assert!(snapshot.trail.len() > 2, "拖尾点太少: {}", snapshot.trail.len());

        sim.pointer_up(t);
        // 松手后 1 秒内仍在活动状态
        let later = t + 0.5;
        sim.advance(later);
        assert!(sim.active());
        let _ = sim.snapshot(viewport, later);

        for frame in 0..90 {
            let frame_time = later + frame as f64 * 0.016;
            sim.advance(frame_time);
            sim.on_frame_rendered(frame_time);
        }
        assert!(!sim.active(), "超过回收窗口后应当停止");
    }
}
