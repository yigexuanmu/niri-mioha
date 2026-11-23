//! 光标特效的运行时状态：把 niri 的指针事件翻译成 BA 模拟的输入。
//!
//! P6：模拟内核从「全局一份」改为「每输出一份」。上游每个显示器会话都有自己
//! 的 overlay / viewport / 模拟实例（`main.cpp:1941` 逐会话推进），点击落在屏幕
//! 交界处时特效会跨两块屏连续；原先全局一份 + 只在指针屏绘制，会让爆裂圆盘和
//! 环在屏幕边缘被切掉一半，跨屏拖尾还会因为 `viewport.height` 突变而整体缩放
//! （N16）。这里用 `Output::name()` 作为 key，各输出独立推进、独立回收。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use super::ba::curve::{ClickParticleSettings, ShardParticleSettings};
use super::ba::sim::DEFAULT_SEED;
use super::ba::{
    apply_visual_config, LayerVisibility, Point, SimulationRuntime, Snapshot, Viewport,
    VisualConfig,
};
use super::effect_target::EffectTarget;
use super::render::TrailTarget;

/// 默认输出 key（离线工具 / 单元测试 / 尚未拿到 Output 名字时使用）。
pub const DEFAULT_OUTPUT_KEY: &str = "default";

/// 拖尾的默认输入采样率（Hz）。P0 起与上游一致：0 = 每个指针事件都采样。
pub const DEFAULT_TRAIL_REFRESH_RATE: u32 = 0;

/// 鼠标键种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Side,
    Extra,
    Task,
}

impl TriggerButton {
    const fn bit(self) -> u8 {
        match self {
            Self::Left => 1 << 0,
            Self::Right => 1 << 1,
            Self::Middle => 1 << 2,
            Self::Back => 1 << 3,
            Self::Forward => 1 << 4,
            Self::Side => 1 << 5,
            Self::Extra => 1 << 6,
            Self::Task => 1 << 7,
        }
    }
}

/// 哪些按键会触发点击爆裂（位集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickTrigger {
    mask: u8,
}

impl ClickTrigger {
    pub const fn empty() -> Self {
        Self { mask: 0 }
    }

    pub const fn single(button: TriggerButton) -> Self {
        Self { mask: button.bit() }
    }

    /// 上游默认：左键 + 右键（`ClickTriggerType::Both`）。
    pub const fn legacy_default() -> Self {
        Self {
            mask: TriggerButton::Left.bit() | TriggerButton::Right.bit(),
        }
    }

    pub fn with(self, button: TriggerButton) -> Self {
        Self {
            mask: self.mask | button.bit(),
        }
    }

    pub fn from_buttons<I: IntoIterator<Item = TriggerButton>>(buttons: I) -> Self {
        buttons
            .into_iter()
            .fold(Self::empty(), |acc, b| acc.with(b))
    }

    pub fn accepts(self, button: TriggerButton) -> bool {
        self.mask & button.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.mask == 0
    }
}

/// 单个输出的运行状态。
struct Slot {
    runtime: SimulationRuntime,
    viewport: Viewport,
    visible: Cell<bool>,
    last_input_sample_at: Option<f64>,
    trail_target: Rc<RefCell<TrailTarget>>,
    effect_target: Rc<RefCell<EffectTarget>>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            runtime: SimulationRuntime::new(DEFAULT_SEED),
            viewport: Viewport::new(1920, 1080),
            visible: Cell::new(true),
            last_input_sample_at: None,
            trail_target: Rc::new(RefCell::new(TrailTarget::default())),
            effect_target: Rc::new(RefCell::new(EffectTarget::default())),
        }
    }
}

impl Slot {
    fn accept_input_sample(&mut self, refresh_rate: u32, time: f64) -> bool {
        if refresh_rate == 0 {
            return true;
        }
        let interval = 1.0 / f64::from(refresh_rate);
        match self.last_input_sample_at {
            Some(last) if time - last < interval => false,
            _ => {
                self.last_input_sample_at = Some(time);
                true
            }
        }
    }
}

/// 光标特效运行时状态（每输出一份模拟）。
pub struct CursorEffectState {
    /// 总开关（`toggle-cursor-effect` 动作切换）。
    pub enabled: bool,
    runtimes: RefCell<HashMap<String, Slot>>,
    /// 模拟时间原点；所有时间戳都是相对它的秒数。
    epoch: Instant,
    visual: VisualConfig,
    click_particle: ClickParticleSettings,
    shard_particle: ShardParticleSettings,
    click_trigger: ClickTrigger,
    pub persistent_trail: bool,
    pub hide_in_fullscreen: bool,
    pub show_on_desktop: bool,
    trail_length: f32,
    click_time_scale: f32,
    trail_time_scale: f32,
    trail_refresh_rate: u32,
    touch_mode: bool,
}

impl Default for CursorEffectState {
    fn default() -> Self {
        Self {
            enabled: true,
            runtimes: RefCell::new(HashMap::new()),
            epoch: Instant::now(),
            visual: VisualConfig::default(),
            click_particle: ClickParticleSettings::default(),
            shard_particle: ShardParticleSettings::default(),
            click_trigger: ClickTrigger::legacy_default(),
            persistent_trail: false,
            hide_in_fullscreen: false,
            show_on_desktop: true,
            trail_length: 1.0,
            click_time_scale: 1.0,
            trail_time_scale: 1.0,
            trail_refresh_rate: DEFAULT_TRAIL_REFRESH_RATE,
            touch_mode: false,
        }
    }
}

impl CursorEffectState {
    fn with_slot<R>(&self, key: &str, f: impl FnOnce(&mut Slot) -> R) -> R {
        let mut runtimes = self.runtimes.borrow_mut();
        let slot = runtimes.entry(key.to_owned()).or_default();
        f(slot)
    }

    fn slot_ref(&self, key: &str) -> Option<std::cell::Ref<'_, HashMap<String, Slot>>> {
        let runtimes = self.runtimes.borrow();
        if runtimes.contains_key(key) {
            Some(runtimes)
        } else {
            None
        }
    }

    /// 应用一份配置（热重载也会走这里）；所有输出共享视觉参数。
    pub fn apply_settings(&mut self, settings: CursorEffectSettings) {
        self.enabled = settings.enabled;
        self.visual = settings.visual;
        self.click_particle = settings.click_particle;
        self.shard_particle = settings.shard_particle;
        self.click_trigger = settings.click_trigger;
        self.persistent_trail = settings.persistent_trail;
        self.hide_in_fullscreen = settings.hide_in_fullscreen;
        self.show_on_desktop = settings.show_on_desktop;
        self.trail_length = settings.trail_length;
        self.click_time_scale = settings.click_time_scale;
        self.trail_time_scale = settings.trail_time_scale;
        let rate_changed = self.trail_refresh_rate != settings.trail_refresh_rate;
        self.trail_refresh_rate = settings.trail_refresh_rate;
        self.touch_mode = settings.touch_mode;

        let now = self.time(Instant::now());
        for slot in self.runtimes.borrow_mut().values_mut() {
            if rate_changed {
                slot.last_input_sample_at = None;
            }
            slot.runtime
                .set_click_particle_settings(self.click_particle);
            slot.runtime
                .set_shard_particle_settings(self.shard_particle);
            slot.runtime.set_trail_length_multiplier(self.trail_length);
            slot.runtime
                .set_click_time_scale(self.click_time_scale, now);
            slot.runtime
                .set_trail_time_scale(self.trail_time_scale, now);
            slot.runtime
                .update_unity_trail_time_scale(self.trail_time_scale);
            slot.runtime
                .set_always_on_trail_enabled(self.persistent_trail, now);
        }
    }

    /// 更新某个输出的物理尺寸（渲染与事件必须用同一份）。
    pub fn set_viewport_for(&self, key: &str, viewport: Viewport) {
        self.with_slot(key, |slot| slot.viewport = viewport);
    }

    /// 兼容单输出调用点 / 测试。
    pub fn set_viewport(&self, viewport: Viewport) {
        self.set_viewport_for(DEFAULT_OUTPUT_KEY, viewport);
    }

    pub fn viewport_for(&self, key: &str) -> Viewport {
        self.slot_ref(key)
            .and_then(|runtimes| runtimes.get(key).map(|slot| slot.viewport))
            .unwrap_or_else(|| Viewport::new(1920, 1080))
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport_for(DEFAULT_OUTPUT_KEY)
    }

    /// 由渲染路径告知某个输出本帧是否会真的绘制。
    pub fn set_visible_for(&self, key: &str, visible: bool) {
        self.with_slot(key, |slot| slot.visible.set(visible));
    }

    /// 兼容单输出调用点 / 测试：把当前所有输出的可见性一起设置。
    pub fn set_visible(&self, visible: bool) {
        for slot in self.runtimes.borrow().values() {
            slot.visible.set(visible);
        }
    }

    pub fn visible(&self) -> bool {
        self.slot_ref(DEFAULT_OUTPUT_KEY)
            .and_then(|runtimes| {
                runtimes
                    .get(DEFAULT_OUTPUT_KEY)
                    .map(|slot| slot.visible.get())
            })
            .unwrap_or(true)
    }

    pub fn touch_mode(&self) -> bool {
        self.enabled && self.touch_mode
    }

    pub fn visual(&self) -> &VisualConfig {
        &self.visual
    }

    pub fn trail_target_for(&self, key: &str) -> Rc<RefCell<TrailTarget>> {
        self.with_slot(key, |slot| slot.trail_target.clone())
    }

    pub fn effect_target_for(&self, key: &str) -> Rc<RefCell<EffectTarget>> {
        self.with_slot(key, |slot| slot.effect_target.clone())
    }

    pub fn trail_target(&self) -> Rc<RefCell<TrailTarget>> {
        self.trail_target_for(DEFAULT_OUTPUT_KEY)
    }

    pub fn effect_target(&self) -> Rc<RefCell<EffectTarget>> {
        self.effect_target_for(DEFAULT_OUTPUT_KEY)
    }

    pub fn set_time_origin(&mut self, origin: Instant) {
        self.epoch = origin;
    }

    fn time(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.epoch).as_secs_f64()
    }

    /// 指针按下：触发点击爆裂（广播到某个输出）。
    pub fn on_button_press_for(
        &mut self,
        key: &str,
        pos: Point,
        viewport: Viewport,
        now: Instant,
        button: TriggerButton,
    ) {
        if !self.enabled {
            return;
        }
        if !self.click_trigger.accepts(button) {
            return;
        }
        let time = self.time(now);
        self.with_slot(key, |slot| {
            slot.viewport = viewport;
            slot.runtime.pointer_down(pos, viewport, time);
            // 按下等于重开一个采样相位：按下之后的第一帧移动永远会被接受。
            slot.last_input_sample_at = None;
        });
    }

    /// 兼容单输出调用点 / 测试。
    pub fn on_button_press(&mut self, pos: Point, now: Instant, button: TriggerButton) {
        self.on_button_press_for(DEFAULT_OUTPUT_KEY, pos, self.viewport(), now, button);
    }

    /// 指针抬起（广播到某个输出）。
    pub fn on_button_release_for(&mut self, key: &str, now: Instant) {
        if !self.enabled {
            return;
        }
        let time = self.time(now);
        self.with_slot(key, |slot| slot.runtime.pointer_up(time));
    }

    pub fn on_button_release(&mut self, now: Instant) {
        self.on_button_release_for(DEFAULT_OUTPUT_KEY, now);
    }

    /// 指针取消（Alt+Tab、指针离开输出、触摸取消）：立即收尾（N11）。
    pub fn on_pointer_cancel_for(&mut self, key: &str, now: Instant) {
        if !self.enabled {
            return;
        }
        let time = self.time(now);
        self.with_slot(key, |slot| {
            slot.runtime.pointer_cancel(time);
            slot.last_input_sample_at = None;
        });
    }

    pub fn on_pointer_cancel(&mut self, now: Instant) {
        self.on_pointer_cancel_for(DEFAULT_OUTPUT_KEY, now);
    }

    /// 指针移动（广播到某个输出）。
    pub fn on_move_for(&mut self, key: &str, pos: Point, viewport: Viewport, now: Instant) {
        if !self.enabled {
            return;
        }
        let time = self.time(now);
        let refresh_rate = self.trail_refresh_rate;
        let persistent = self.persistent_trail;
        self.with_slot(key, |slot| {
            slot.viewport = viewport;
            if !slot.accept_input_sample(refresh_rate, time) {
                return;
            }
            slot.runtime.set_always_on_trail_enabled(persistent, time);
            slot.runtime.pointer_move(pos, viewport, time);
        });
    }

    pub fn on_move(&mut self, pos: Point, now: Instant) {
        self.on_move_for(DEFAULT_OUTPUT_KEY, pos, self.viewport(), now);
    }

    /// 兼容单输出测试：采样门。
    #[cfg(test)]
    fn accept_input_sample(&mut self, time: f64) -> bool {
        let refresh_rate = self.trail_refresh_rate;
        self.with_slot(DEFAULT_OUTPUT_KEY, |slot| {
            slot.accept_input_sample(refresh_rate, time)
        })
    }

    /// 每帧推进**所有**输出；返回是否还有可绘制内容。
    pub fn advance_all(&mut self, now: Instant) -> bool {
        if !self.enabled {
            return false;
        }
        let time = self.time(now);
        let mut alive = false;
        for slot in self.runtimes.borrow_mut().values_mut() {
            if !slot.visible.get() {
                // 看不见就不需要继续模拟；顺手清掉，避免隐藏期间累积出一个
                // 「永远活着」的实例让合成器永久空转（N21）。
                if slot.runtime.active() {
                    slot.runtime.discard_active_effects();
                }
                continue;
            }
            slot.runtime.advance(time);
            if slot.runtime.rendering_required(time) {
                alive = true;
            }
        }
        alive
    }

    /// 兼容单输出调用点 / 测试。
    pub fn advance(&mut self, now: Instant) -> bool {
        self.advance_all(now)
    }

    /// 渲染完一帧后调用（对应上游 `onFrameRendered`，负责回收）。
    pub fn on_frame_rendered(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        let time = self.time(now);
        for slot in self.runtimes.borrow_mut().values_mut() {
            slot.runtime.on_frame_rendered(time);
            // N23：没有活动实例时释放离屏纹理，避免 4K 下几十 MB 常驻。
            if !slot.runtime.active() {
                slot.trail_target.borrow_mut().release();
                slot.effect_target.borrow_mut().release();
            }
        }
    }

    /// 取某个输出的一帧快照并套用产品级视觉参数。
    pub fn snapshot_for(&self, key: &str, viewport: Viewport, now: Instant) -> Snapshot {
        let time = self.time(now);
        let mut snapshot = self.with_slot(key, |slot| {
            slot.viewport = viewport;
            slot.runtime.snapshot(viewport, time)
        });
        apply_visual_config(&mut snapshot, &self.visual);
        snapshot
    }

    pub fn snapshot(&self, viewport: Viewport, now: Instant) -> Snapshot {
        self.snapshot_for(DEFAULT_OUTPUT_KEY, viewport, now)
    }

    /// 关掉特效时清空全部粒子与拖尾。
    pub fn clear(&mut self) {
        for slot in self.runtimes.borrow_mut().values_mut() {
            slot.runtime.discard_active_effects();
        }
    }

    pub fn has_content(&self) -> bool {
        self.runtimes
            .borrow()
            .values()
            .any(|slot| slot.runtime.active())
    }

    /// 是否还需要继续出帧（任一可见输出仍有活动实例）。
    pub fn has_work_any(&self) -> bool {
        self.enabled
            && self
                .runtimes
                .borrow()
                .values()
                .any(|slot| slot.visible.get() && slot.runtime.active())
    }

    pub fn has_work(&self) -> bool {
        self.has_work_any()
    }

    pub fn set_layer_visibility(&mut self, visibility: LayerVisibility) {
        for slot in self.runtimes.borrow_mut().values_mut() {
            slot.runtime.set_layer_visibility(visibility);
        }
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
    pub persistent_trail: bool,
    pub hide_in_fullscreen: bool,
    pub show_on_desktop: bool,
    pub trail_length: f32,
    pub click_time_scale: f32,
    pub trail_time_scale: f32,
    pub trail_refresh_rate: u32,
    pub touch_mode: bool,
}

impl Default for CursorEffectSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            visual: VisualConfig::default(),
            click_particle: ClickParticleSettings::default(),
            shard_particle: ShardParticleSettings::default(),
            click_trigger: ClickTrigger::legacy_default(),
            persistent_trail: false,
            hide_in_fullscreen: false,
            show_on_desktop: true,
            trail_length: 1.0,
            click_time_scale: 1.0,
            trail_time_scale: 1.0,
            trail_refresh_rate: DEFAULT_TRAIL_REFRESH_RATE,
            touch_mode: false,
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
        state.on_button_press(Point::new(960.0, 540.0), start, TriggerButton::Left);
        state.advance(start + std::time::Duration::from_millis(16));
        let snapshot = state.snapshot(
            Viewport::new(1920, 1080),
            start + std::time::Duration::from_millis(16),
        );
        assert!(!snapshot.sprites.is_empty());
    }

    #[test]
    fn trigger_filter_respects_config() {
        let legacy = ClickTrigger::legacy_default();
        assert!(legacy.accepts(TriggerButton::Left));
        assert!(legacy.accepts(TriggerButton::Right));
        assert!(!legacy.accepts(TriggerButton::Middle));

        let custom = ClickTrigger::from_buttons([
            TriggerButton::Right,
            TriggerButton::Back,
            TriggerButton::Forward,
        ]);
        assert!(custom.accepts(TriggerButton::Right));
        assert!(custom.accepts(TriggerButton::Back));
        assert!(custom.accepts(TriggerButton::Forward));
        assert!(!custom.accepts(TriggerButton::Left));
        assert!(!custom.accepts(TriggerButton::Middle));

        assert!(ClickTrigger::empty().is_empty());
        assert!(!ClickTrigger::empty().accepts(TriggerButton::Left));
    }

    #[test]
    fn non_trigger_button_does_not_burst() {
        let mut state = CursorEffectState::default();
        state.set_viewport(Viewport::new(1920, 1080));
        let start = Instant::now();
        state.epoch = start;
        state.click_trigger = ClickTrigger::from_buttons([TriggerButton::Right]);

        state.on_button_press(Point::new(960.0, 540.0), start, TriggerButton::Left);
        assert!(!state.has_work(), "左键不在列表里，不该起爆");

        state.on_button_press(Point::new(960.0, 540.0), start, TriggerButton::Right);
        assert!(state.has_work(), "右键在列表里，应该起爆");
    }

    #[test]
    fn per_output_instances_are_independent() {
        let mut state = CursorEffectState::default();
        let start = Instant::now();
        state.epoch = start;
        // 虚拟双屏：同一世界坐标 (1920, 540) 落在左屏右边缘 / 右屏左边缘。
        state.on_button_press_for(
            "eDP-1",
            Point::new(1920.0, 540.0),
            Viewport::new(1920, 1080),
            start,
            TriggerButton::Left,
        );
        state.on_button_press_for(
            "DP-1",
            Point::new(0.0, 540.0),
            Viewport::new(2560, 1440),
            start,
            TriggerButton::Left,
        );
        state.advance_all(start + std::time::Duration::from_millis(16));
        let left = state.snapshot_for(
            "eDP-1",
            Viewport::new(1920, 1080),
            start + std::time::Duration::from_millis(16),
        );
        let right = state.snapshot_for(
            "DP-1",
            Viewport::new(2560, 1440),
            start + std::time::Duration::from_millis(16),
        );
        assert!(!left.sprites.is_empty());
        assert!(!right.sprites.is_empty());
        // 各自视口的 ref_scale 不同（1440p = 1.333），不是同一份全局模拟。
        assert!((left.ref_scale - 1.0).abs() < 1e-4);
        assert!((right.ref_scale - 1440.0 / 1080.0).abs() < 1e-4);
        assert!(right.ref_scale > left.ref_scale);
        // 交界处连续：左屏盘心在 1920（右边缘），右屏盘心在 0（左边缘）。
        let center = |snapshot: &Snapshot| {
            snapshot
                .sprites
                .iter()
                .find(|sprite| sprite.kind == crate::cursor_effect::ba::SpriteKind::CenterDisk)
                .expect("CenterDisk")
                .center
        };
        assert!((center(&left).x - 1920.0).abs() < 2.0);
        assert!(center(&right).x.abs() < 2.0);
    }

    #[test]
    fn has_work_respects_visibility() {
        let mut state = CursorEffectState::default();
        state.set_viewport(Viewport::new(1920, 1080));
        let start = Instant::now();
        state.epoch = start;
        state.on_button_press(Point::new(100.0, 100.0), start, TriggerButton::Left);
        state.advance(start + std::time::Duration::from_millis(16));
        assert!(state.has_work(), "可见时点击应保持出帧");

        state.set_visible(false);
        assert!(!state.has_work(), "不可见时不该保持出帧");
        assert!(!state.advance(start + std::time::Duration::from_millis(32)));
        assert!(!state.has_content(), "不可见时应就地清空，避免永久空转");

        state.set_visible(true);
        assert!(!state.has_work(), "清空后没有实例，不该继续出帧");
    }

    #[test]
    fn input_sampling_gate_follows_rate() {
        let mut state = CursorEffectState {
            trail_refresh_rate: 40,
            ..Default::default()
        };
        let start = Instant::now();
        state.epoch = start;
        let at = |ms: u64| start + std::time::Duration::from_millis(ms);

        state.on_button_press(Point::new(0.0, 0.0), at(0), TriggerButton::Left);
        let mut accepted = 0;
        for ms in (0..100).step_by(4) {
            if state.accept_input_sample(ms as f64 / 1000.0) {
                accepted += 1;
            }
        }
        assert!((3..=5).contains(&accepted), "accepted={accepted}");

        state.trail_refresh_rate = 0;
        assert!(state.accept_input_sample(0.101));
        assert!(state.accept_input_sample(0.102));
    }
}
