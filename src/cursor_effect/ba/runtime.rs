//! 上游 `SimulationRuntime` 的移植：每一次点击都活在自己的 [`Simulation`] 实例里。
//!
//! 之前这里只持有一个 `Simulation`，而 `Simulation::pointer_down()` 会调用
//! `reset_state()` 清空 `rings` / `triangles` / `trail` —— 也就是说「再点一下」
//! 会把上一次尚未播完的爆裂和拖尾碎片直接抹掉，看起来就是「前一个特效瞬间消失、
//! 不连贯」。上游的桌面端并不是这么跑的：`SimulationRuntime` 维护一组实例，
//! 按下时从「Unity 组件池」里取一个（取不到就新建），旧实例继续按自己的时间线
//! 播完，`on_frame_rendered()` 里再把不活跃的实例回收进池子。
//!
//! 另一个关键点：池化实例**保留** `trailTime`（Unity 侧 `FxTrailTimeScale`
//! 是兄弟组件，`FXTouch.Stop` 不会重置它），但把时间锚点重新落在此刻，于是
//! 新实例的拖尾时间戳能从旧值继续往上走，不会和别人撞在一起。
//!
//! 除了实例管理，这里还移植了「常驻拖尾」（上游 `alwaysOnTrail`）：不按键时
//! 单独起一个只带拖尾的实例，一旦真按下就把它退休成普通实例（保留淡出），
//! 由新的按下实例接管指针。

use std::collections::VecDeque;

use super::curve::{ClickParticleSettings, ShardParticleSettings};
use super::sim::{
    LayerVisibility, Point, Simulation, Snapshot, TrailStroke, Viewport, DEFAULT_SEED,
};

/// 池化种子步长（与上游 `randomStreamStep` 一致）。
const RANDOM_STREAM_STEP: u64 = 0x9E37_79B9_7F4A_7C15;
/// 常驻拖尾的独立随机流。
const AMBIENT_RANDOM_STREAM: u64 = 0xD1B5_4A32_D192_ED03;

/// 一个正在运行的实例。
struct Instance {
    simulation: Simulation,
    /// 是否回收进「Unity 组件池」。常驻拖尾不是从池里拿的，退役时只保留淡出。
    returns_to_pool: bool,
}

/// 点击特效运行时：实例集合 + 组件池 + 常驻拖尾。
pub struct SimulationRuntime {
    base_seed: u64,
    instances: Vec<Instance>,
    unity_pool: VecDeque<Simulation>,
    unity_activation_count: u64,
    ambient_activation_count: u64,
    /// 是否有一个实例正持有指针（Unity 同时只持有一个对象）。
    pointer_active: bool,
    always_on_trail_enabled: bool,
    always_on_trail: Option<Simulation>,
    click_particle_settings: ClickParticleSettings,
    shard_particle_settings: ShardParticleSettings,
    trail_length_multiplier: f32,
    layer_visibility: LayerVisibility,
    click_time_scale: f32,
    trail_time_scale: f32,
}

impl Default for SimulationRuntime {
    fn default() -> Self {
        Self::new(DEFAULT_SEED)
    }
}

impl SimulationRuntime {
    pub fn new(seed: u64) -> Self {
        let mut runtime = Self {
            base_seed: seed,
            instances: Vec::with_capacity(8),
            unity_pool: VecDeque::with_capacity(4),
            unity_activation_count: 0,
            ambient_activation_count: 0,
            pointer_active: false,
            always_on_trail_enabled: false,
            always_on_trail: None,
            click_particle_settings: ClickParticleSettings::default(),
            shard_particle_settings: ShardParticleSettings::default(),
            trail_length_multiplier: 1.0,
            layer_visibility: LayerVisibility::default(),
            click_time_scale: 1.0,
            trail_time_scale: 1.0,
        };
        // `TouchEffectCreater` 初始化 `SyncComponentPool<FXTouch>` 时就有一个成员，
        // 先造出来，好让第一次激活也走同一条「从池里取」的路径。
        runtime.unity_pool.push_back(Simulation::new(seed));
        runtime
    }

    /// 指针按下：新开一个实例，旧实例继续播完。
    pub fn pointer_down(&mut self, screen_position: Point, viewport: Viewport, time: f64) {
        // Unity 对「当前按住的指针」只保留一个对象，重复的 down 不能替换它，
        // 也不能重启它的粒子。
        if self.pointer_active {
            tracing::debug!("cursor-effect: pointer_down ignored, still holding");
            return;
        }
        let instance = self.acquire_pressed_instance(time);
        instance
            .simulation
            .pointer_down(screen_position, viewport, time);
        self.pointer_active = true;
        tracing::debug!(
            "cursor-effect: pointer_down at {:?} viewport={:?} instances={} pooled={}",
            screen_position,
            viewport,
            self.instances.len(),
            self.unity_pool.len()
        );
    }

    pub fn pointer_move(&mut self, screen_position: Point, viewport: Viewport, time: f64) {
        if self.pointer_active {
            if let Some(instance) = self.instances.last_mut() {
                instance
                    .simulation
                    .pointer_move(screen_position, viewport, time);
            }
            return;
        }

        if !self.always_on_trail_enabled {
            return;
        }
        if self.always_on_trail.is_none() {
            // 第一个自由移动样本只当锚点：把它接上「切换前」或屏幕外的坐标会
            // 画出一条虚假的长线。
            let seed = self.next_ambient_seed();
            let mut simulation = Simulation::new(seed);
            simulation.set_trail_length_multiplier(self.trail_length_multiplier);
            simulation.set_layer_visibility(self.layer_visibility);
            simulation.set_click_time_scale(self.click_time_scale, time);
            simulation.set_trail_time_scale(self.trail_time_scale, time);
            simulation.set_shard_particle_settings(self.shard_particle_settings);
            simulation.start_trail(screen_position, viewport, time);
            self.always_on_trail = Some(simulation);
            return;
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.pointer_move(screen_position, viewport, time);
        }
    }

    pub fn pointer_up(&mut self, time: f64) {
        if !self.pointer_active {
            return;
        }
        if let Some(instance) = self.instances.last_mut() {
            instance.simulation.pointer_up(time);
        }
        self.pointer_active = false;
    }

    pub fn pointer_cancel(&mut self, time: f64) {
        if self.pointer_active {
            if let Some(instance) = self.instances.last_mut() {
                instance.simulation.pointer_cancel(time);
            }
            self.pointer_active = false;
        }
        self.retire_always_on_trail(time);
    }

    /// 硬关闭（改模式、关特效）：全部实例与池一起丢掉。
    pub fn discard_active_effects(&mut self) {
        self.pointer_active = false;
        self.instances.clear();
        self.always_on_trail = None;
        // 池化实例可能留着 `FxTrailTimeScale` 的停放状态，硬关之后复用会带着
        // 旧主人的几何回来。
        self.unity_pool.clear();
    }

    pub fn end_always_on_trail(&mut self, time: f64) {
        self.retire_always_on_trail(time);
    }

    pub fn set_always_on_trail_enabled(&mut self, enabled: bool, time: f64) {
        self.always_on_trail_enabled = enabled;
        if !enabled {
            self.retire_always_on_trail(time);
        }
    }

    pub fn always_on_trail_active(&self) -> bool {
        self.always_on_trail.is_some()
    }

    pub fn advance(&mut self, time: f64) {
        for instance in &mut self.instances {
            instance.simulation.advance(time);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.advance(time);
        }
    }

    pub fn on_frame_rendered(&mut self, time: f64) {
        for instance in &mut self.instances {
            instance.simulation.on_frame_rendered(time);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.on_frame_rendered(time);
        }

        let mut index = 0;
        while index < self.instances.len() {
            if self.instances[index].simulation.active() {
                index += 1;
                continue;
            }
            let instance = self.instances.remove(index);
            if instance.returns_to_pool {
                // `ComponentPool<T>.AddObject` 是尾部入队；按激活顺序遍历就能
                // 在多个协程同帧结束时复现游戏的 FIFO 回收顺序。
                self.unity_pool.push_back(instance.simulation);
            }
        }
    }

    pub fn update_unity_trail_time_scale(&mut self, time_scale: f32) {
        for instance in &mut self.instances {
            instance
                .simulation
                .update_unity_trail_time_scale(time_scale);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.update_unity_trail_time_scale(time_scale);
        }
    }

    pub fn set_click_time_scale(&mut self, time_scale: f32, time: f64) {
        self.click_time_scale = time_scale;
        for instance in &mut self.instances {
            instance.simulation.set_click_time_scale(time_scale, time);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.set_click_time_scale(time_scale, time);
        }
    }

    pub fn set_trail_time_scale(&mut self, time_scale: f32, time: f64) {
        self.trail_time_scale = time_scale;
        for instance in &mut self.instances {
            instance.simulation.set_trail_time_scale(time_scale, time);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.set_trail_time_scale(time_scale, time);
        }
    }

    pub fn set_click_particle_settings(&mut self, settings: ClickParticleSettings) {
        self.click_particle_settings = settings;
        for instance in &mut self.instances {
            instance.simulation.set_click_particle_settings(settings);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.set_click_particle_settings(settings);
        }
    }

    pub fn set_shard_particle_settings(&mut self, settings: ShardParticleSettings) {
        self.shard_particle_settings = settings;
        for instance in &mut self.instances {
            instance.simulation.set_shard_particle_settings(settings);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.set_shard_particle_settings(settings);
        }
    }

    pub fn set_trail_length_multiplier(&mut self, multiplier: f32) {
        self.trail_length_multiplier = multiplier;
        for instance in &mut self.instances {
            instance.simulation.set_trail_length_multiplier(multiplier);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.set_trail_length_multiplier(multiplier);
        }
    }

    pub fn set_layer_visibility(&mut self, visibility: LayerVisibility) {
        self.layer_visibility = visibility;
        for instance in &mut self.instances {
            instance.simulation.set_layer_visibility(visibility);
        }
        if let Some(ambient) = self.always_on_trail.as_mut() {
            ambient.set_layer_visibility(visibility);
        }
    }

    /// 合并所有实例的快照。
    ///
    /// 拖尾按实例分别成「笔画」收集：把两条不同时刻的轨迹首尾接起来会画出
    /// 一条并不存在的连线。
    pub fn snapshot(&self, viewport: Viewport, time: f64) -> Snapshot {
        let mut combined = Snapshot {
            global_opacity: 1.0,
            trail_opacity: 1.0,
            ref_scale: super::sim::reference_scale(viewport),
            ..Default::default()
        };

        for instance in &self.instances {
            Self::merge(&mut combined, instance.simulation.snapshot(viewport, time));
        }
        if let Some(ambient) = self.always_on_trail.as_ref() {
            Self::merge(&mut combined, ambient.snapshot(viewport, time));
        }

        // 单笔画字段留最新的一条（渲染层走 `trail_strokes`，这里只为兼容）。
        if let Some(newest) = combined.trail_strokes.last() {
            combined.trail = newest.points.clone();
            combined.trail_width = newest.width;
        }

        combined.sprites.sort_by_key(|sprite| sprite.render_queue);
        combined
    }

    fn merge(combined: &mut Snapshot, mut current: Snapshot) {
        combined.active |= current.active;
        combined.pointer_held |= current.pointer_held;
        combined.sprites.append(&mut current.sprites);
        combined.trail_strokes.append(&mut current.trail_strokes);
        if current.trail_strokes.is_empty() && current.trail.len() >= 2 {
            combined.trail_strokes.push(TrailStroke {
                points: current.trail,
                width: current.trail_width,
            });
        }
    }

    pub fn rendering_required(&self, time: f64) -> bool {
        if !self.instances.is_empty() {
            return true;
        }
        self.always_on_trail
            .as_ref()
            .is_some_and(|ambient| ambient.has_drawable_content(time))
    }

    pub fn active(&self) -> bool {
        !self.instances.is_empty() || self.always_on_trail.is_some()
    }

    pub fn pointer_held(&self) -> bool {
        self.pointer_active
    }

    pub fn instance_count(&self) -> usize {
        self.instances.len() + usize::from(self.always_on_trail.is_some())
    }

    pub fn pooled_instance_count(&self) -> usize {
        self.unity_pool.len()
    }

    fn next_unity_seed(&mut self) -> u64 {
        let seed = self
            .base_seed
            .wrapping_add(self.unity_activation_count.wrapping_mul(RANDOM_STREAM_STEP));
        self.unity_activation_count += 1;
        seed
    }

    fn next_ambient_seed(&mut self) -> u64 {
        let seed = (self.base_seed ^ AMBIENT_RANDOM_STREAM).wrapping_add(
            self.ambient_activation_count
                .wrapping_mul(RANDOM_STREAM_STEP),
        );
        self.ambient_activation_count += 1;
        seed
    }

    fn acquire_pressed_instance(&mut self, time: f64) -> &mut Instance {
        // 一次真实的按下接管指针，移动专用的常驻拖尾就此退休 —— 保留它的淡出，
        // 但不让两套几何重复画。
        self.retire_always_on_trail(time);

        let seed = self.next_unity_seed();
        match self.unity_pool.pop_front() {
            None => self.instances.push(Instance {
                simulation: Simulation::new(seed),
                returns_to_pool: true,
            }),
            Some(mut simulation) => {
                simulation.prepare_pooled_activation(seed);
                self.instances.push(Instance {
                    simulation,
                    returns_to_pool: true,
                });
            }
        }

        let instance = self.instances.last_mut().expect("just pushed");
        // 与生成数量/半径相关的设置必须在池复用**之后**、`pointer_down` 采样
        // 之前装好，否则这一下会用旧实例的参数。
        instance
            .simulation
            .set_click_particle_settings(self.click_particle_settings);
        instance
            .simulation
            .set_shard_particle_settings(self.shard_particle_settings);
        instance
            .simulation
            .set_trail_length_multiplier(self.trail_length_multiplier);
        instance
            .simulation
            .set_layer_visibility(self.layer_visibility);
        instance
            .simulation
            .set_click_time_scale(self.click_time_scale, time);
        instance
            .simulation
            .set_trail_time_scale(self.trail_time_scale, time);
        instance
    }

    fn retire_always_on_trail(&mut self, time: f64) {
        let Some(mut ambient) = self.always_on_trail.take() else {
            return;
        };
        ambient.pointer_cancel(time);
        // 常驻拖尾是桌面端增强，不是从游戏组件池里取的 `FXTouch`：留着淡出，
        // 但不进池子。
        self.instances.push(Instance {
            simulation: ambient,
            returns_to_pool: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Viewport {
        Viewport::new(1920, 1080)
    }

    /// 再点一下不能把上一次的爆裂抹掉。
    #[test]
    fn second_click_keeps_the_previous_burst() {
        let mut runtime = SimulationRuntime::default();
        runtime.pointer_down(Point::new(960.0, 540.0), viewport(), 0.0);
        runtime.advance(0.016);
        runtime.pointer_up(0.05);
        let first = runtime.snapshot(viewport(), 0.1);
        assert!(!first.sprites.is_empty());

        // 第二个点击落在别处，第一个仍在播。
        runtime.pointer_down(Point::new(600.0, 400.0), viewport(), 0.12);
        runtime.advance(0.13);
        let merged = runtime.snapshot(viewport(), 0.13);
        assert_eq!(runtime.instance_count(), 2);
        assert!(
            merged.sprites.len() > first.sprites.len() / 2,
            "旧爆裂被清掉了: {} -> {}",
            first.sprites.len(),
            merged.sprites.len()
        );

        // 两个爆裂的中心都在：一个在 (960,540)，一个在 (600,400)。
        let near = |x: f32, y: f32| {
            merged
                .sprites
                .iter()
                .any(|s| (s.center.x - x).abs() < 2.0 && (s.center.y - y).abs() < 2.0)
        };
        assert!(near(960.0, 540.0), "第一个圆盘不见了");
        assert!(near(600.0, 400.0), "第二个圆盘不见了");
    }

    /// 播完的实例要回到池子里，池子复用后不能丢掉「新种子」。
    #[test]
    fn finished_instances_are_pooled() {
        let mut runtime = SimulationRuntime::default();
        runtime.pointer_down(Point::new(960.0, 540.0), viewport(), 0.0);
        runtime.pointer_up(0.05);
        let mut time = 0.05;
        for _ in 0..400 {
            time += 1.0 / 60.0;
            runtime.advance(time);
            runtime.on_frame_rendered(time);
        }
        assert_eq!(runtime.instance_count(), 0);
        assert!(runtime.pooled_instance_count() >= 1);
    }

    /// 合并快照时要按绘制顺序排，且两条独立拖尾不能连成一条。
    #[test]
    fn snapshot_merges_sprites_in_render_queue_order() {
        let mut runtime = SimulationRuntime::default();
        runtime.set_always_on_trail_enabled(true, 0.0);
        runtime.pointer_move(Point::new(100.0, 100.0), viewport(), 0.0); // 锚点
        runtime.pointer_move(Point::new(300.0, 100.0), viewport(), 0.05);
        runtime.advance(0.06);

        runtime.pointer_down(Point::new(900.0, 500.0), viewport(), 0.1);
        runtime.advance(0.11);

        let snapshot = runtime.snapshot(viewport(), 0.11);
        let queues: Vec<i32> = snapshot.sprites.iter().map(|s| s.render_queue).collect();
        assert!(queues.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(snapshot.trail_strokes.len() >= 1);
    }
}
