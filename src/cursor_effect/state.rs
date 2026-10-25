//! 光标特效的运行时状态：把 niri 的指针事件翻译成 BA 模拟的输入。

use std::cell::Cell;
use std::time::Instant;

use super::ba::curve::{ClickParticleSettings, ShardParticleSettings};
use super::ba::sim::DEFAULT_SEED;
use super::ba::{
    apply_visual_config, LayerVisibility, Point, SimulationRuntime, Snapshot, Viewport,
    VisualConfig,
};

/// 哪些按键会触发点击爆裂。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTrigger {
    Left,
    Right,
    Both,
}

impl ClickTrigger {
    pub fn accepts(self, left: bool, right: bool) -> bool {
        match self {
            Self::Left => left,
            Self::Right => right,
            Self::Both => left || right,
        }
    }
}

/// 光标特效运行时状态。
pub struct CursorEffectState {
    /// 总开关（`toggle-cursor-effect` 动作切换）。
    pub enabled: bool,
    runtime: SimulationRuntime,
    /// 模拟时间原点；所有时间戳都是相对它的秒数。
    epoch: Instant,
    /// 鼠标所在输出的物理尺寸，事件与渲染共用同一份，保证坐标自洽。
    viewport: Cell<Viewport>,
    visual: VisualConfig,
    click_particle: ClickParticleSettings,
    shard_particle: ShardParticleSettings,
    click_trigger: ClickTrigger,
    middle_click_trigger: bool,
    /// 不按键也常显拖尾。
    pub persistent_trail: bool,
    pub hide_in_fullscreen: bool,
    pub show_on_desktop: bool,
    trail_length: f32,
    click_time_scale: f32,
    trail_time_scale: f32,
}

impl Default for CursorEffectState {
    fn default() -> Self {
        Self {
            enabled: true,
            runtime: SimulationRuntime::new(DEFAULT_SEED),
            epoch: Instant::now(),
            viewport: Cell::new(Viewport::new(1920, 1080)),
            visual: VisualConfig::default(),
            click_particle: ClickParticleSettings::default(),
            shard_particle: ShardParticleSettings::default(),
            click_trigger: ClickTrigger::Left,
            middle_click_trigger: true,
            persistent_trail: false,
            hide_in_fullscreen: true,
            show_on_desktop: true,
            trail_length: 1.0,
            click_time_scale: 1.0,
            trail_time_scale: 1.0,
        }
    }
}

impl CursorEffectState {
    /// 应用一份配置（热重载也会走这里）。
    pub fn apply_settings(&mut self, settings: CursorEffectSettings) {
        self.enabled = settings.enabled;
        self.visual = settings.visual;
        self.click_particle = settings.click_particle;
        self.shard_particle = settings.shard_particle;
        self.click_trigger = settings.click_trigger;
        self.middle_click_trigger = settings.middle_click_trigger;
        self.persistent_trail = settings.persistent_trail;
        self.hide_in_fullscreen = settings.hide_in_fullscreen;
        self.show_on_desktop = settings.show_on_desktop;
        self.trail_length = settings.trail_length;
        self.click_time_scale = settings.click_time_scale;
        self.trail_time_scale = settings.trail_time_scale;

        let now = self.time(Instant::now());
        self.runtime
            .set_click_particle_settings(self.click_particle);
        self.runtime
            .set_shard_particle_settings(self.shard_particle);
        self.runtime.set_trail_length_multiplier(self.trail_length);
        self.runtime
            .set_click_time_scale(self.click_time_scale, now);
        self.runtime
            .set_trail_time_scale(self.trail_time_scale, now);
        self.runtime
            .update_unity_trail_time_scale(self.trail_time_scale);
        // 常驻拖尾换到独立实例，配置变化要立刻同步过去。
        self.runtime
            .set_always_on_trail_enabled(self.persistent_trail, now);
    }

    /// 更新鼠标所在输出尺寸（物理像素）。渲染与事件必须用同一份。
    pub fn set_viewport(&self, viewport: Viewport) {
        self.viewport.set(viewport);
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport.get()
    }

    fn time(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.epoch).as_secs_f64()
    }

    /// 指针按下：触发点击爆裂。
    pub fn on_button_press(&mut self, pos: Point, now: Instant, left: bool, middle: bool) {
        if !self.enabled {
            return;
        }
        let right = !left && !middle;
        let accepted = if middle {
            self.middle_click_trigger
        } else {
            self.click_trigger.accepts(left, right)
        };
        if !accepted {
            return;
        }
        let time = self.time(now);
        self.runtime.pointer_down(pos, self.viewport.get(), time);
    }

    /// 指针抬起。
    pub fn on_button_release(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.runtime.pointer_up(self.time(now));
    }

    /// 指针移动：按住时喂拖尾；常驻拖尾模式下自行起一个只带拖尾的实例。
    pub fn on_move(&mut self, pos: Point, now: Instant) {
        if !self.enabled {
            return;
        }
        let time = self.time(now);
        let viewport = self.viewport.get();
        // 按下时喂当前实例；没按下时（常驻拖尾）由运行时自己维护一个只带拖尾的
        // 实例，上游 `SimulationRuntime::pointerMove` 同构。
        self.runtime
            .set_always_on_trail_enabled(self.persistent_trail, time);
        self.runtime.pointer_move(pos, viewport, time);
    }

    /// 每帧推进；返回是否还有可绘制内容（调用方据此决定是否持续出帧）。
    pub fn advance(&mut self, now: Instant) -> bool {
        if !self.enabled {
            return false;
        }
        let time = self.time(now);
        self.runtime.advance(time);
        self.runtime.rendering_required(time)
    }

    /// 渲染完一帧后调用（对应上游 `onFrameRendered`，负责回收）。
    pub fn on_frame_rendered(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        let time = self.time(now);
        self.runtime.on_frame_rendered(time);
    }

    /// 取一帧快照并套用产品级视觉参数。
    pub fn snapshot(&self, viewport: Viewport, now: Instant) -> Snapshot {
        let mut snapshot = self.runtime.snapshot(viewport, self.time(now));
        apply_visual_config(&mut snapshot, &self.visual);
        snapshot
    }

    /// 关掉特效时清空全部粒子与拖尾。
    pub fn clear(&mut self) {
        self.runtime.discard_active_effects();
    }

    pub fn has_content(&self) -> bool {
        self.runtime.active()
    }

    /// 是否还需要继续出帧（对应上游「模拟仍激活」）。
    pub fn has_work(&self) -> bool {
        self.enabled && self.runtime.active()
    }

    pub fn set_layer_visibility(&mut self, visibility: LayerVisibility) {
        self.runtime.set_layer_visibility(visibility);
    }
}

/// 一次性下发的配置集合（由 niri 配置层翻译而来）。
#[derive(Debug, Clone, Copy)]
pub struct CursorEffectSettings {
    pub enabled: bool,
    pub visual: VisualConfig,
    pub click_particle: ClickParticleSettings,
    pub shard_particle: ShardParticleSettings,
    pub click_trigger: ClickTrigger,
    pub middle_click_trigger: bool,
    pub persistent_trail: bool,
    pub hide_in_fullscreen: bool,
    pub show_on_desktop: bool,
    pub trail_length: f32,
    pub click_time_scale: f32,
    pub trail_time_scale: f32,
}

impl Default for CursorEffectSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            visual: VisualConfig::default(),
            click_particle: ClickParticleSettings::default(),
            shard_particle: ShardParticleSettings::default(),
            click_trigger: ClickTrigger::Left,
            middle_click_trigger: true,
            persistent_trail: false,
            hide_in_fullscreen: true,
            show_on_desktop: true,
            trail_length: 1.0,
            click_time_scale: 1.0,
            trail_time_scale: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_produces_snapshot() {
        let mut state = CursorEffectState::default();
        state.set_viewport(Viewport::new(1920, 1080));
        let start = Instant::now();
        state.epoch = start;
        state.on_button_press(Point::new(960.0, 540.0), start, true, false);
        state.advance(start + std::time::Duration::from_millis(16));
        let snapshot = state.snapshot(
            Viewport::new(1920, 1080),
            start + std::time::Duration::from_millis(16),
        );
        assert!(!snapshot.sprites.is_empty());
    }

    #[test]
    fn trigger_filter_respects_config() {
        assert!(ClickTrigger::Both.accepts(true, false));
        assert!(!ClickTrigger::Left.accepts(false, true));
    }
}
