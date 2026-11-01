//! 光标特效的运行时状态：把 niri 的指针事件翻译成 BA 模拟的输入。

use std::cell::{Cell, RefCell};
use std::time::Instant;

use super::ba::curve::{ClickParticleSettings, ShardParticleSettings};
use super::ba::sim::DEFAULT_SEED;
use super::ba::{
    apply_visual_config, LayerVisibility, Point, SimulationRuntime, Snapshot, Viewport,
    VisualConfig,
};
use super::render::TrailTarget;

/// 拖尾的默认输入采样率（Hz），与 `niri-config` 的
/// `CursorEffect::default().trail_refresh_rate` 一致。
///
/// 两处必须同值：实机永远走 `settings_from_config`（拿的是 40），而离线渲染
/// 工具和单元测试走 `CursorEffectSettings::default()`。一旦两份漂移，测出来的
/// 东西就不代表用户看到的东西了。
pub const DEFAULT_TRAIL_REFRESH_RATE: u32 = 40;

/// 鼠标键种类。
///
/// 刻意**不**直接用 smithay 的 `MouseButton`：这一层是纯逻辑，单元测试要能在
/// 没有后端类型的情况下跑；输入层负责把 `MouseButton` 翻成这个枚举。
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
    /// 在集合里的位序号。8 个键正好一个字节。
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

    pub const ALL: [TriggerButton; 8] = [
        Self::Left,
        Self::Right,
        Self::Middle,
        Self::Back,
        Self::Forward,
        Self::Side,
        Self::Extra,
        Self::Task,
    ];
}

/// 哪些按键会触发点击爆裂（位集合）。
///
/// 上游 BASpark 的 `ClickTriggerType` 只有「左 / 右 / 两者」三档，中键靠一个
/// 单独的 `enable-middle-click-trigger` 开关，侧键和后两个键根本不认。配置项
/// `trigger-buttons` 把这三种写法合成一个显式列表，这里是它在运行时的形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickTrigger {
    mask: u8,
}

impl ClickTrigger {
    /// 一个键都不触发（`trigger-buttons` 写成空节点）。
    pub const fn empty() -> Self {
        Self { mask: 0 }
    }

    pub const fn single(button: TriggerButton) -> Self {
        Self {
            mask: button.bit(),
        }
    }

    /// 上游默认：左键 + 中键（`click-trigger left` + `enable-middle-click-trigger`）。
    pub const fn legacy_default() -> Self {
        Self {
            mask: TriggerButton::Left.bit() | TriggerButton::Middle.bit(),
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
    /// 不按键也常显拖尾。
    pub persistent_trail: bool,
    pub hide_in_fullscreen: bool,
    pub show_on_desktop: bool,
    trail_length: f32,
    click_time_scale: f32,
    trail_time_scale: f32,
    /// 拖尾的输入采样率（Hz）。0 = 每个指针事件都采。
    trail_refresh_rate: u32,
    /// 上一次被接受的输入采样时刻（模拟时间，秒）。
    last_input_sample_at: Option<f64>,
    /// 是否让触摸输入驱动特效（`touch-mode`）。
    touch_mode: bool,
    /// 拖尾的离屏合成目标（`GL_MAX` 并集，见 `render::TrailTarget`）。
    ///
    /// GPU 资源放在状态里而不是每次都新建：离屏纹理只增不减，一帧一次分配会
    /// 稳定地每帧触发一次显存分配 / 释放。
    trail_target: RefCell<TrailTarget>,
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
            click_trigger: ClickTrigger::legacy_default(),
            persistent_trail: false,
            hide_in_fullscreen: true,
            show_on_desktop: true,
            trail_length: 1.0,
            click_time_scale: 1.0,
            trail_time_scale: 1.0,
            trail_refresh_rate: DEFAULT_TRAIL_REFRESH_RATE,
            last_input_sample_at: None,
            touch_mode: false,
            trail_target: RefCell::new(TrailTarget::default()),
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
        self.persistent_trail = settings.persistent_trail;
        self.hide_in_fullscreen = settings.hide_in_fullscreen;
        self.show_on_desktop = settings.show_on_desktop;
        self.trail_length = settings.trail_length;
        self.click_time_scale = settings.click_time_scale;
        self.trail_time_scale = settings.trail_time_scale;
        if self.trail_refresh_rate != settings.trail_refresh_rate {
            // 换采样率等于换一整条相位，旧的采样时刻不能再用。
            self.last_input_sample_at = None;
        }
        self.trail_refresh_rate = settings.trail_refresh_rate;
        self.touch_mode = settings.touch_mode;

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

    /// 触摸输入是否要驱动特效（供输入层判断）。
    pub fn touch_mode(&self) -> bool {
        self.enabled && self.touch_mode
    }

    /// 当前视觉参数。渲染层要用到 `glow_*` / `ring_inner_ratio` / `tint` 这些
    /// 只影响着色、不影响模拟的量，它们没有进 `Snapshot`（上游快照结构固定），
    /// 所以由渲染层直接借用。
    pub fn visual(&self) -> &VisualConfig {
        &self.visual
    }

    /// 拖尾的离屏合成目标（渲染层用）。
    pub fn trail_target(&self) -> &RefCell<TrailTarget> {
        &self.trail_target
    }

    /// 对齐内部时间原点（离线渲染工具 / 基准测试用）。
    ///
    /// `time(now)` 返回 `now - epoch` 的秒数。工具侧把自己的时钟锚点传进来，
    /// `time(anchor + t)` 就**恰好**等于 `t`。不锚定的话，`epoch`（状态创建的
    /// 真实时刻）与工具时钟之间夹着一段真实创建耗时，纳秒级的运行间抖动会让
    /// 40Hz 输入采样门在 `0.025s` 的判定边界上随机翻转 —— 实测拖尾末端凭空
    /// 差出 2~3 个采样点（约 60px），亮暗两态随机切换。
    pub fn set_time_origin(&mut self, origin: Instant) {
        self.epoch = origin;
    }

    fn time(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.epoch).as_secs_f64()
    }

    /// 指针按下：触发点击爆裂。
    pub fn on_button_press(&mut self, pos: Point, now: Instant, button: TriggerButton) {
        if !self.enabled {
            return;
        }
        if !self.click_trigger.accepts(button) {
            return;
        }
        let time = self.time(now);
        self.runtime.pointer_down(pos, self.viewport.get(), time);
        // 按下等于重开一个采样相位：上游 `acquirePressedInstance` 在派发
        // `pointerDown` 之后会把 `lastInputSampleAt_` 重置成按下时刻，这样按下
        // 之后的第一帧移动永远会被接受（不管是鼠标还是刚落到屏上的手指）。
        self.last_input_sample_at = None;
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
        if !self.accept_input_sample(time) {
            return;
        }
        let viewport = self.viewport.get();
        // 按下时喂当前实例；没按下时（常驻拖尾）由运行时自己维护一个只带拖尾的
        // 实例，上游 `SimulationRuntime::pointerMove` 同构。
        self.runtime
            .set_always_on_trail_enabled(self.persistent_trail, time);
        self.runtime.pointer_move(pos, viewport, time);
    }

    /// 输入采样门（对应上游 `SimulationRuntime::acceptInputSample`）。
    ///
    /// `trail-refresh-rate` 就是上游的 `inputSamplingRateHz`：> 0 时按固定频率
    /// 丢弃指针事件，只有跨过一个采样间隔的那次移动才会喂给模拟。0 表示「每个
    /// 事件都采」，也就是旧行为。
    ///
    /// 采样被丢弃时**整个** `pointer_move` 都不调用（上游也是直接 return）：
    /// 拖尾点是按采样时刻追加的，只更新位置而不追加点会让新旧两点之间的线段
    /// 长度与时间对不上，带宽和颜色的进度插值跟着歪。
    fn accept_input_sample(&mut self, time: f64) -> bool {
        if self.trail_refresh_rate == 0 {
            return true;
        }
        let interval = 1.0 / f64::from(self.trail_refresh_rate);
        match self.last_input_sample_at {
            Some(last) if time - last < interval => false,
            _ => {
                self.last_input_sample_at = Some(time);
                true
            }
        }
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
    pub persistent_trail: bool,
    pub hide_in_fullscreen: bool,
    pub show_on_desktop: bool,
    pub trail_length: f32,
    pub click_time_scale: f32,
    pub trail_time_scale: f32,
    /// 拖尾输入采样率（Hz）。0 = 每个指针事件都采。
    pub trail_refresh_rate: u32,
    /// 触摸输入是否驱动特效（`touch-mode`）。
    ///
    /// 关掉时光标特效只跟鼠标/触摸板走 —— 触摸屏上按下手指不会有任何效果。
    /// 打开后 `on_touch_down` / `on_touch_motion` / `on_touch_up` 也会喂事件，
    /// 于是触摸屏能画出同样的拖尾和点击爆裂。
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
            hide_in_fullscreen: true,
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
        // 上游默认：左键 + 中键。
        let legacy = ClickTrigger::legacy_default();
        assert!(legacy.accepts(TriggerButton::Left));
        assert!(legacy.accepts(TriggerButton::Middle));
        assert!(!legacy.accepts(TriggerButton::Right));
        assert!(!legacy.accepts(TriggerButton::Back));

        // 显式列表：只认写出来的那几个键。
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

        // 空列表 = 一个键都不触发。
        assert!(ClickTrigger::empty().is_empty());
        assert!(!ClickTrigger::empty().accepts(TriggerButton::Left));
    }

    /// 配置里写了 `trigger-buttons` 之后，按下列表外的键不能起爆。
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

    /// 输入采样门：40Hz 下 100ms 内只放行约 4 个事件；按下重置相位，所以
    /// 按下后的第一次移动永远被接受（上游 `firstAdvancePending` 的豁免）。
    #[test]
    fn input_sampling_gate_follows_rate() {
        let mut state = CursorEffectState::default();
        state.trail_refresh_rate = 40;
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
        // 0、~28、~52、~80 → 4 次。
        assert!((3..=5).contains(&accepted), "accepted={accepted}");

        // 0Hz = 关闭门限，每个事件都采。
        state.trail_refresh_rate = 0;
        assert!(state.accept_input_sample(0.101));
        assert!(state.accept_input_sample(0.102));
    }
}
