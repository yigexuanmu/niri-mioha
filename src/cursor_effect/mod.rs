//! 光标特效（Cursor Effects）模块。
//!
//! 点击爆裂 + 拖动拖尾，移植自
//! [`CialloKing/ba-click-fx-desktop`](https://github.com/CialloKing/ba-click-fx-desktop)
//! （《蔚蓝档案》的 FX_Touch 点击特效重建）。粒子模拟、曲线、纹理都来自上游，
//! 这里只负责接上 niri 的指针事件与 GLES 渲染。

pub mod ba;
pub mod effect_target;
pub mod render;
pub mod state;

use std::time::Instant;

use niri_config::misc::{ClickTriggerButton, ClickTriggerType, CursorEffect as CursorEffectConfig};
use smithay::utils::{Logical, Point, Size};
pub use state::{ClickTrigger, CursorEffectSettings, CursorEffectState, TriggerButton};

use self::ba::{Point as FxPoint, Viewport};
use crate::niri::Niri;

/// 把全局逻辑坐标的指针位置换成「输出本地物理像素」，同时给出该输出的物理尺寸。
///
/// 模拟视口就是输出自身的物理尺寸，所以特效坐标天然是输出本地的，渲染时不需要
/// 再做全局坐标换算 —— 但多显示器下必须用指针所在的那块屏，否则尺寸不一致会让
/// 拖尾和爆裂按错误的比例铺开。
pub fn output_local_pointer(
    niri: &Niri,
    global: Point<f64, Logical>,
) -> Option<(FxPoint, Viewport)> {
    let (output, within) = niri.output_under(global)?;
    let scale = output.current_scale().fractional_scale();
    let position = within.to_physical(scale);
    let size: Size<i32, smithay::utils::Physical> = match output.current_mode() {
        Some(mode) => mode.size,
        None => {
            let logical = niri.global_space.output_geometry(output)?.size;
            Size::from((
                (logical.w as f64 * scale).round().max(1.0) as i32,
                (logical.h as f64 * scale).round().max(1.0) as i32,
            ))
        }
    };
    Some((
        FxPoint::new(position.x as f32, position.y as f32),
        Viewport::new(size.w.max(1) as u32, size.h.max(1) as u32),
    ))
}

/// 指定输出上的指针坐标与视口（P6）。与 `output_local_pointer` 不同，这里不
/// 要求指针正好落在这块输出上：负坐标 / 越界坐标会原样保留，交给视口裁剪，
/// 这样点击在屏幕交界处时，相邻输出的实例也能画出自己那一半。
pub fn output_pointer_for(
    niri: &Niri,
    output: &smithay::output::Output,
    global: Point<f64, Logical>,
) -> Option<(FxPoint, Viewport)> {
    let geometry = niri.global_space.output_geometry(output)?;
    let scale = output.current_scale().fractional_scale();
    let within = global - geometry.loc.to_f64();
    let position = within.to_physical(scale);
    let size: Size<i32, smithay::utils::Physical> = match output.current_mode() {
        Some(mode) => mode.size,
        None => {
            let logical = geometry.size;
            Size::from((
                (logical.w as f64 * scale).round().max(1.0) as i32,
                (logical.h as f64 * scale).round().max(1.0) as i32,
            ))
        }
    };
    Some((
        FxPoint::new(position.x as f32, position.y as f32),
        Viewport::new(size.w.max(1) as u32, size.h.max(1) as u32),
    ))
}

/// 当前所有输出的 key（用于抬起 / 取消广播）。
fn output_keys(niri: &Niri) -> Vec<String> {
    niri.global_space
        .outputs()
        .map(|output| output.name())
        .collect()
}

impl Niri {
    /// 指针按下：把同一世界坐标广播到所有输出（P6）。
    ///
    /// 上游每个显示器会话都有独立的模拟实例；点击落在交界处时两块屏都会画出
    /// 自己那一半爆裂，而不是被裁掉。这里用 `Output::name()` 路由。
    pub fn cursor_effect_pointer_down(
        &mut self,
        global: Point<f64, Logical>,
        now: Instant,
        button: TriggerButton,
    ) {
        let targets: Vec<(String, FxPoint, Viewport)> = self
            .global_space
            .outputs()
            .filter_map(|output| {
                let (pos, viewport) = output_pointer_for(self, output, global)?;
                Some((output.name(), pos, viewport))
            })
            .collect();
        for (key, pos, viewport) in targets {
            self.cursor_effect
                .on_button_press_for(&key, pos, viewport, now, button);
        }
    }

    /// 指针移动：同样广播到所有输出，跨屏拖尾因此连续。
    pub fn cursor_effect_pointer_move(&mut self, global: Point<f64, Logical>, now: Instant) {
        let targets: Vec<(String, FxPoint, Viewport)> = self
            .global_space
            .outputs()
            .filter_map(|output| {
                let (pos, viewport) = output_pointer_for(self, output, global)?;
                Some((output.name(), pos, viewport))
            })
            .collect();
        for (key, pos, viewport) in targets {
            self.cursor_effect.on_move_for(&key, pos, viewport, now);
        }
    }

    /// 指针抬起：所有输出一起结束拖尾。
    pub fn cursor_effect_pointer_up(&mut self, now: Instant) {
        for key in output_keys(self) {
            self.cursor_effect.on_button_release_for(&key, now);
        }
    }

    /// 指针取消：所有输出立即收尾。
    pub fn cursor_effect_pointer_cancel(&mut self, now: Instant) {
        for key in output_keys(self) {
            self.cursor_effect.on_pointer_cancel_for(&key, now);
        }
    }
}

/// 环境门控的纯函数形式。
///
/// 原先把「全屏隐藏 / 桌面隐藏 / 锁屏 / 输出匹配」四件事内联在渲染路径里，
/// 既不可单测，也容易在搬家时漏掉某一条。抽成纯函数之后，L1 可以跑完整的
/// 7 布尔矩阵，断言「锁定必不画」「指针可见性不参与判定」。
pub fn effect_should_render(
    enabled: bool,
    hide_in_fullscreen: bool,
    show_on_desktop: bool,
    under_fullscreen: bool,
    under_window: bool,
    locked: bool,
    output_matches: bool,
) -> bool {
    if !enabled || locked || !output_matches {
        return false;
    }
    if hide_in_fullscreen && under_fullscreen {
        return false;
    }
    if !under_window && !show_on_desktop {
        return false;
    }
    true
}

impl Niri {
    /// 把 `cursor-effect` 配置翻译成模拟参数。
    ///
    /// 映射说明：
    /// - `enabled` → 中控开关；`opacity` → 全局不透明度。
    /// - `scale` → 全局尺寸缩放，**1.0 才是上游原尺寸**（默认 1.5 会放大 1.5 倍； 想 1:1
    ///   复刻上游就写 `scale 1.0`）。
    /// - `effect-speed`（或 `use-linked-animation-speed false` 时的 `trail-speed` /
    ///   `click-speed`）→ 点击/拖尾时间尺度。
    /// - `enable-always-trail` → 不按键也画拖尾；`click-trigger` / `enable-middle-click-trigger` →
    ///   哪些按键触发爆裂。
    /// - `hide-in-fullscreen` / `show-on-desktop` → 环境过滤。
    /// - `tint` → 整体着色乘数（`255,255,255` 是原样）。
    /// - `disk` / `rings` / `shards` / `trail` / `glow` 五个子块 → 各图元的
    ///   尺寸、寿命、速度、透明度、辉光。全部是 1080p 参考像素 / 毫秒。
    /// - 上游的 `color` 字段已从 schema 删除（它从未参与渲染，N10）；着色请用 `tint`。
    /// - `trail-refresh-rate` → 拖尾的输入采样率（上游 `inputSamplingRateHz`）：> 0
    ///   时按该频率采样指针事件，0 表示每个事件都采。
    /// - `apply-curve-draw` → 拖尾条带是否用 Catmull-Rom 补点（见 `VisualConfig::curve_draw`）。
    /// - `touch-mode` → 是否让触摸输入也驱动特效（见 `CursorEffectState::touch_mode`）。
    pub fn apply_cursor_effect_config(&mut self, cfg: &CursorEffectConfig) {
        self.cursor_effect.apply_settings(settings_from_config(cfg));
    }

    /// 热重载路径：配置里的 `enabled` 只是**初始**状态，运行时的
    /// `ToggleCursorEffect` 开关优先级更高（P5）。
    pub fn apply_cursor_effect_config_reloading(&mut self, cfg: &CursorEffectConfig) {
        let enabled = self.cursor_effect.enabled;
        self.cursor_effect.apply_settings(settings_from_config(cfg));
        self.cursor_effect.enabled = enabled;
    }

    /// 每帧推进光标特效状态机；返回 `true` 表示仍有活动粒子，调用方据此决定
    /// 是否即便无其它 damage 也提交一帧。
    ///
    /// 注意这里**不再**调用 `on_frame_rendered`：推进发生在快照之前，回收必须
    /// 发生在该帧真正提交之后（N17）。否则动画最后一帧会被提前回收，跳帧时
    /// 还可能整段不显示。两个后端在提交成功后调用
    /// [`Niri::cursor_effect_frame_rendered`]。
    pub fn advance_cursor_effect(&mut self, now: Instant) -> bool {
        self.cursor_effect.advance(now)
    }

    /// 一帧已经提交（或已被离线工具消费）之后调用，回收播完的实例。
    pub fn cursor_effect_frame_rendered(&mut self, now: Instant) {
        self.cursor_effect.on_frame_rendered(now);
    }
}

/// 配置里的鼠标键名 → 运行时的 `TriggerButton`。
fn trigger_button_from_config(button: ClickTriggerButton) -> TriggerButton {
    match button {
        ClickTriggerButton::Left => TriggerButton::Left,
        ClickTriggerButton::Right => TriggerButton::Right,
        ClickTriggerButton::Middle => TriggerButton::Middle,
        ClickTriggerButton::Back => TriggerButton::Back,
        ClickTriggerButton::Forward => TriggerButton::Forward,
        ClickTriggerButton::Side => TriggerButton::Side,
        ClickTriggerButton::Extra => TriggerButton::Extra,
        ClickTriggerButton::Task => TriggerButton::Task,
    }
}

/// 合成「哪些键触发点击爆裂」。
///
/// `trigger-buttons` 只要出现过就**完全接管**：写 `trigger-buttons "right"` 就
/// 只有右键有效，不会再被 `click-trigger left` 或 `enable-middle-click-trigger`
/// 悄悄加回左/中键 —— 否则用户没法真正关掉某个键。三项都没写时按上游默认
/// （左键 + 中键）。
fn click_trigger_from_config(cfg: &CursorEffectConfig) -> ClickTrigger {
    if let Some(buttons) = &cfg.trigger_buttons {
        return ClickTrigger::from_buttons(buttons.iter().map(|b| trigger_button_from_config(*b)));
    }
    let mut trigger = match cfg.click_trigger {
        ClickTriggerType::Left => ClickTrigger::single(TriggerButton::Left),
        ClickTriggerType::Right => ClickTrigger::single(TriggerButton::Right),
        ClickTriggerType::Both => {
            ClickTrigger::from_buttons([TriggerButton::Left, TriggerButton::Right])
        }
    };
    if cfg.enable_middle_click_trigger {
        trigger = trigger.with(TriggerButton::Middle);
    }
    trigger
}

/// 上游对每个数值参数都做过归一化（`simulation.cpp:105-160`）。niri 侧原先
/// 完全没有这层，配置里多打一个零就能让合成器一次分配 50 万个环粒子，或者让
/// 拖尾寿命涨到 5.5 小时（N19）。这里照抄上游常量做一次防御性钳制。
fn finite_clamp(value: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        min
    }
}

const LIFETIME_MIN_MS: f64 = 1.0;
const LIFETIME_MAX_MS: f64 = 10_000.0;
const EFFECT_RADIUS_MAX: f64 = 2_000.0;
const RING_SPIN_MAX: f64 = 100.0;

/// 把 `niri-config` 的 `cursor-effect` 解析结果翻译成模拟参数。
///
/// 抽成自由函数是为了让离线渲染工具（`examples/ce_scene.rs`）能走**同一条**
/// 映射路径：如果只在这里内联，工具就得自己复制一份换算，两份一旦漂移，
/// 「渲染出来的东西」和「用户配置出来的东西」就不是一回事了。
pub fn settings_from_config(cfg: &CursorEffectConfig) -> CursorEffectSettings {
    let (trail_speed, click_speed) = if cfg.use_linked_animation_speed {
        (cfg.effect_speed, cfg.effect_speed)
    } else {
        (cfg.trail_speed, cfg.click_speed)
    };

    CursorEffectSettings {
        enabled: cfg.enabled,
        visual: ba::VisualConfig {
            opacity: finite_clamp(cfg.opacity, 0.0, 1.0) as f32,
            global_scale: finite_clamp(cfg.scale, 0.0, 64.0) as f32,
            disk_radius: finite_clamp(cfg.disk.radius, 0.0, EFFECT_RADIUS_MAX) as f32,
            trail_width: finite_clamp(cfg.trail.width, 0.0, 64.0) as f32,
            trail_opacity: finite_clamp(cfg.trail.opacity, 0.0, 1.0) as f32,
            rings_hdr_intensity: finite_clamp(cfg.rings.hdr_intensity, 0.0, 64.0) as f32,
            shards_hdr_intensity: finite_clamp(cfg.shards.hdr_intensity, 0.0, 64.0) as f32,
            glow_gain: finite_clamp(cfg.glow.gain, 0.0, 64.0) as f32,
            glow_trail_gain: finite_clamp(cfg.glow.trail_gain, 0.0, 64.0) as f32,
            glow_radius: finite_clamp(cfg.glow.radius, 0.0, 64.0) as f32,
            ring_inner_ratio: finite_clamp(cfg.rings.inner_ratio, 0.0, 0.999_9) as f32,
            tint: [
                finite_clamp(cfg.tint.0[0] as f64 / 255.0, 0.0, 1.0) as f32,
                finite_clamp(cfg.tint.0[1] as f64 / 255.0, 0.0, 1.0) as f32,
                finite_clamp(cfg.tint.0[2] as f64 / 255.0, 0.0, 1.0) as f32,
            ],
            curve_draw: cfg.apply_curve_draw,
        },
        click_particle: ba::curve::ClickParticleSettings {
            disk_lifetime_ms: finite_clamp(cfg.disk.lifetime, LIFETIME_MIN_MS, LIFETIME_MAX_MS)
                as f32,
            rings_count: cfg.rings.count.min(64),
            rings_lifetime_ms: finite_clamp(cfg.rings.lifetime, LIFETIME_MIN_MS, LIFETIME_MAX_MS)
                as f32,
            rings_radius_min: finite_clamp(cfg.rings.radius_min, 0.0, EFFECT_RADIUS_MAX) as f32,
            rings_radius_max: finite_clamp(cfg.rings.radius_max, 0.0, EFFECT_RADIUS_MAX) as f32,
            rings_angular_velocity_multiplier: finite_clamp(cfg.rings.spin, 0.0, RING_SPIN_MAX)
                as f32,
            rings_rotation_direction: finite_clamp(cfg.rings.spin_direction, -1.0, 1.0) as f32,
        },
        shard_particle: ba::curve::ShardParticleSettings {
            click_count: cfg.shards.count.min(1000),
            click_lifetime_min_ms: finite_clamp(
                cfg.shards.lifetime_min,
                LIFETIME_MIN_MS,
                LIFETIME_MAX_MS,
            ) as f32,
            click_lifetime_max_ms: finite_clamp(
                cfg.shards.lifetime_max,
                LIFETIME_MIN_MS,
                LIFETIME_MAX_MS,
            ) as f32,
            click_radius: finite_clamp(cfg.shards.radius, 0.0, EFFECT_RADIUS_MAX) as f32,
            click_speed_min: finite_clamp(cfg.shards.speed_min, 0.0, EFFECT_RADIUS_MAX) as f32,
            click_speed_max: finite_clamp(cfg.shards.speed_max, 0.0, EFFECT_RADIUS_MAX) as f32,
            size_min: finite_clamp(cfg.shards.size_min, 0.0, EFFECT_RADIUS_MAX) as f32,
            size_max: finite_clamp(cfg.shards.size_max, 0.0, EFFECT_RADIUS_MAX) as f32,
        },
        click_trigger: click_trigger_from_config(cfg),
        persistent_trail: cfg.enable_always_trail,
        hide_in_fullscreen: cfg.hide_in_fullscreen,
        show_on_desktop: cfg.show_on_desktop,
        trail_length: finite_clamp(cfg.trail.length, 0.0, 33.3) as f32,
        click_time_scale: finite_clamp(click_speed, 0.0, 100.0) as f32,
        trail_time_scale: finite_clamp(trail_speed, 0.0, 100.0) as f32,
        trail_refresh_rate: cfg.trail_refresh_rate,
        touch_mode: cfg.touch_mode,
    }
}

#[cfg(test)]
mod structure_tests {
    use super::effect_should_render;

    /// N22：特效不能再被搬回 `render_pointer`（它会让指针隐藏 = 特效消失）。
    #[test]
    fn effect_not_inside_render_pointer() {
        let src = include_str!("../niri.rs");
        let rp = src.split("pub fn render_pointer").nth(1).unwrap();
        let body = &rp[..rp.find("\n    pub fn ").unwrap_or(rp.len())];
        assert!(
            !body.contains("collect_render_elements"),
            "特效又被打回 render_pointer 里了（N22）"
        );
        assert!(
            src.contains("fn push_cursor_effect"),
            "内容层的特效入口不见了"
        );
    }

    /// N20：两个后端都必须在渲染前推进模拟，否则 winit 下永远不会出图。
    #[test]
    fn advance_called_from_both_backends() {
        let tty = include_str!("../backend/tty.rs");
        let winit = include_str!("../backend/winit.rs");
        assert!(tty.contains("advance_cursor_effect"));
        assert!(
            winit.contains("advance_cursor_effect"),
            "winit 后端没有推进光标特效（N20）"
        );
    }

    /// N22/L3 退路：内容层特效调用必须出现在锁屏分支之前，且锁屏时跳过。
    #[test]
    fn effect_push_is_before_locked_branch() {
        let src = include_str!("../niri.rs");
        let call = src
            .find("self.push_cursor_effect(ctx.renderer, output, push)")
            .expect("render_inner 必须调用 push_cursor_effect");
        assert!(
            src[call..].contains("if self.is_locked()"),
            "锁屏分支必须位于特效调用之后，否则锁屏仍会绘制粒子"
        );
        assert!(
            src[..call].contains("self.cursor_effect.set_visible(false)"),
            "锁屏时必须把可见性置 false（N21）"
        );
    }

    /// N21：`has_work_any` 必须同时检查每个 slot 的可见性与活动实例。
    #[test]
    fn has_work_checks_visibility() {
        let src = include_str!("state.rs");
        assert!(
            src.contains("slot.visible.get() && slot.runtime.active()"),
            "has_work 又只看实例数量了（N21）"
        );
    }

    /// L3 运行时矩阵（T1–T4）：在 state/门控层复现 §12.5 的运行时决策。
    ///
    /// 这不是完整 Niri headless（构造 Niri 需要 State/Wayland/seat），而是与
    /// L1 的 2^7 矩阵、L4 结构守卫互补的运行时验证；T1–T4 的结论都能在本
    /// 测试里直接观察到。
    #[test]
    fn l3_t1_t4_runtime_matrix() {
        // T1：指针隐藏仍出特效。`effect_should_render` 的签名里没有
        // pointer_visibility，任何“光标不可见”状态都无法把结果翻成 false。
        assert!(effect_should_render(
            true, false, true, false, true, false, true
        ));

        // T2：winit/TTY 共同推进路径：点击后 advance_all 必须要求出帧。
        let mut state = crate::cursor_effect::state::CursorEffectState::default();
        let start = std::time::Instant::now();
        state.set_time_origin(start);
        state.on_button_press(
            crate::cursor_effect::ba::Point::new(100.0, 100.0),
            start,
            crate::cursor_effect::state::TriggerButton::Left,
        );
        assert!(
            state.advance_all(start + std::time::Duration::from_millis(16)),
            "T2: advance_all 应返回 true"
        );

        // T3：打字/闲置把可见性置 false 后，不再空转并清空实例。
        state.set_visible(false);
        assert!(!state.has_work_any(), "T3: 不可见时 has_work 必须 false");
        assert!(!state.advance_all(start + std::time::Duration::from_millis(32)));
        assert!(!state.has_content(), "T3: 不可见时应就地清空");

        // T4：全屏隐藏。
        assert!(!effect_should_render(
            true, true, true, true, true, false, true
        ));
        assert!(effect_should_render(
            true, false, true, false, true, false, true
        ));
    }

    /// N14：`trail { width }` 必须作用到真正被渲染的 stroke 宽度。
    #[test]
    fn trail_width_applied_to_strokes() {
        let src = include_str!("ba/mod.rs");
        assert!(
            src.contains("snapshot.trail_strokes"),
            "trail width 又只乘兼容字段了（N14）"
        );
    }
}

#[cfg(test)]
mod tests {
    use niri_config::Config;

    use super::*;

    fn config(text: &str) -> CursorEffectConfig {
        Config::parse_mem(text).unwrap().cursor_effect
    }

    /// 默认配置必须等于「上游原样」：除了 `scale`（上游产品默认 1.5）之外，
    /// 所有视觉参数都应当是恒等值，否则 1:1 复刻的前提就没了。
    #[test]
    fn default_config_maps_to_upstream_defaults() {
        let settings = settings_from_config(&config(""));
        assert!(settings.enabled);
        assert_eq!(settings.visual.tint, [1.0, 1.0, 1.0]);
        assert!((settings.visual.glow_gain - 1.0).abs() < 1e-6);
        assert!((settings.visual.glow_radius - 1.0).abs() < 1e-6);
        assert!((settings.visual.ring_inner_ratio - 0.940_127_5).abs() < 1e-6);
        assert!((settings.visual.disk_radius - 64.8).abs() < 1e-4);
        assert!((settings.trail_length - 1.0).abs() < 1e-6);
        assert!((settings.click_time_scale - 1.0).abs() < 1e-6);
        assert!((settings.trail_time_scale - 1.0).abs() < 1e-6);
        assert_eq!(settings.click_particle.rings_count, 2);
        assert_eq!(settings.shard_particle.click_count, 4);
    }

    /// 上一轮还只是「解析成功但没人用」的三个字段：`trail-refresh-rate` /
    /// `apply-curve-draw` / `touch-mode`。它们现在必须真的落到参数上。
    #[test]
    fn leftover_fields_reach_the_simulation() {
        let settings = settings_from_config(&config(
            r##"
            cursor-effect {
                trail-refresh-rate 0
                apply-curve-draw true
                touch-mode true
            }
            "##,
        ));
        assert_eq!(settings.trail_refresh_rate, 0);
        assert!(settings.visual.curve_draw);
        assert!(settings.touch_mode);

        let default = settings_from_config(&config(""));
        assert_eq!(default.trail_refresh_rate, 0);
        assert!(!default.visual.curve_draw);
        assert!(!default.touch_mode);
        // 两处默认必须一致：实机走 `settings_from_config`（niri-config 的 40），
        // 离线渲染工具和测试走 `CursorEffectSettings::default()`。漂移会让
        // 「测出来的东西」不再代表用户看到的东西。
        assert_eq!(
            CursorEffectSettings::default().trail_refresh_rate,
            default.trail_refresh_rate
        );
    }

    /// 每个 kdl 字段都要真的落到某个参数上，不能「解析成功但没人用」。
    #[test]
    fn tuning_fields_reach_the_simulation() {
        let settings = settings_from_config(&config(
            r##"
            cursor-effect {
                scale 1.0
                opacity 0.5
                tint "#336699"
                disk {
                    radius 30
                    lifetime 111
                }
                rings {
                    count 7
                    lifetime 222
                    radius-min 10
                    radius-max 20
                    spin 3
                    spin-direction 1
                    inner-ratio 0.5
                    hdr-intensity 2
                }
                shards {
                    count 9
                    lifetime-min 300
                    lifetime-max 400
                    radius 5
                    speed-min 6
                    speed-max 7
                    size-min 8
                    size-max 9
                    hdr-intensity 4
                }
                trail {
                    width 2
                    opacity 0.25
                    length 3
                }
                glow {
                    gain 0
                    radius 5
                }
            }
            "##,
        ));

        // 全局
        assert!((settings.visual.global_scale - 1.0).abs() < 1e-6);
        assert!((settings.visual.opacity - 0.5).abs() < 1e-6);
        assert_eq!(
            settings.visual.tint,
            [
                0x33 as f32 / 255.0,
                0x66 as f32 / 255.0,
                0x99 as f32 / 255.0
            ]
        );

        // 圆盘
        assert!((settings.visual.disk_radius - 30.0).abs() < 1e-4);
        assert!((settings.click_particle.disk_lifetime_ms - 111.0).abs() < 1e-4);

        // 环
        assert_eq!(settings.click_particle.rings_count, 7);
        assert!((settings.click_particle.rings_lifetime_ms - 222.0).abs() < 1e-4);
        assert!((settings.click_particle.rings_radius_min - 10.0).abs() < 1e-4);
        assert!((settings.click_particle.rings_radius_max - 20.0).abs() < 1e-4);
        assert!((settings.click_particle.rings_angular_velocity_multiplier - 3.0).abs() < 1e-4);
        assert!((settings.click_particle.rings_rotation_direction - 1.0).abs() < 1e-4);
        assert!((settings.visual.ring_inner_ratio - 0.5).abs() < 1e-6);
        assert!((settings.visual.rings_hdr_intensity - 2.0).abs() < 1e-4);

        // 碎片
        assert_eq!(settings.shard_particle.click_count, 9);
        assert!((settings.shard_particle.click_lifetime_min_ms - 300.0).abs() < 1e-4);
        assert!((settings.shard_particle.click_lifetime_max_ms - 400.0).abs() < 1e-4);
        assert!((settings.shard_particle.click_radius - 5.0).abs() < 1e-4);
        assert!((settings.shard_particle.click_speed_min - 6.0).abs() < 1e-4);
        assert!((settings.shard_particle.click_speed_max - 7.0).abs() < 1e-4);
        assert!((settings.shard_particle.size_min - 8.0).abs() < 1e-4);
        assert!((settings.shard_particle.size_max - 9.0).abs() < 1e-4);
        assert!((settings.visual.shards_hdr_intensity - 4.0).abs() < 1e-4);

        // 拖尾
        assert!((settings.visual.trail_width - 2.0).abs() < 1e-4);
        assert!((settings.visual.trail_opacity - 0.25).abs() < 1e-4);
        assert!((settings.trail_length - 3.0).abs() < 1e-4);

        // 辉光
        assert_eq!(settings.visual.glow_gain, 0.0);
        assert!((settings.visual.glow_radius - 5.0).abs() < 1e-4);
    }

    /// `enabled false` 必须真的把特效关掉（而不仅是「不画」）。
    #[test]
    fn enabled_false_disables_the_effect() {
        let mut state = CursorEffectState::default();
        state.apply_settings(settings_from_config(&config(
            r#"
            cursor-effect {
                enabled false
            }
            "#,
        )));
        assert!(!state.enabled);
        assert!(!state.has_work());

        let start = Instant::now();
        state.on_button_press(FxPoint::new(100.0, 100.0), start, TriggerButton::Left);
        state.advance(start + std::time::Duration::from_millis(32));
        assert!(!state.has_content(), "关闭后不应产生任何粒子");
    }

    /// `trigger-buttons` 的三种写法：不写 / 写几个 / 写成空的。
    #[test]
    fn trigger_buttons_from_config() {
        // 不写 → 上游默认（左键 + 右键）。
        let default = settings_from_config(&config("cursor-effect {}"));
        assert!(default.click_trigger.accepts(TriggerButton::Left));
        assert!(default.click_trigger.accepts(TriggerButton::Right));
        assert!(!default.click_trigger.accepts(TriggerButton::Middle));
        assert!(!default.click_trigger.accepts(TriggerButton::Back));

        // 显式列表**完全接管**，不再被 click-trigger / 中键开关加回去。
        let custom = settings_from_config(&config(
            r#"
            cursor-effect {
                trigger-buttons "right" "back"
            }
            "#,
        ));
        assert!(custom.click_trigger.accepts(TriggerButton::Right));
        assert!(custom.click_trigger.accepts(TriggerButton::Back));
        assert!(!custom.click_trigger.accepts(TriggerButton::Left));
        assert!(!custom.click_trigger.accepts(TriggerButton::Middle));

        // 空节点 = 一个键都不触发（只留常驻拖尾）。
        let none = settings_from_config(&config(
            r#"
            cursor-effect {
                trigger-buttons
            }
            "#,
        ));
        assert!(none.click_trigger.is_empty());

        // 别名与前缀大小写。
        let aliased = settings_from_config(&config(
            r#"
            cursor-effect {
                trigger-buttons "Primary" "MIDDLE" "forward"
            }
            "#,
        ));
        assert!(aliased.click_trigger.accepts(TriggerButton::Left));
        assert!(aliased.click_trigger.accepts(TriggerButton::Middle));
        assert!(aliased.click_trigger.accepts(TriggerButton::Forward));
    }

    /// N19 回归：上游对每个数值参数都钳制，niri 侧不能把 50 万个环粒子
    /// 原样塞进模拟，否则一次点击就能冻结合成器。
    #[test]
    fn settings_clamped_to_upstream_limits() {
        let settings = settings_from_config(&config(
            r#"
            cursor-effect {
                scale 60000
                rings {
                    count 500000
                    lifetime 2000000000
                    radius-min 60000
                    radius-max 60000
                    spin 2000000000
                }
                shards {
                    count 500000
                    lifetime-min 2000000000
                    lifetime-max 2000000000
                }
                trail {
                    length 60000
                }
            }
            "#,
        ));
        assert!(settings.click_particle.rings_count <= 64);
        assert!(settings.shard_particle.click_count <= 1000);
        assert!(settings.click_particle.disk_lifetime_ms <= 10_000.0);
        assert!(settings.click_particle.rings_lifetime_ms <= 10_000.0);
        assert!(settings.shard_particle.click_lifetime_min_ms <= 10_000.0);
        assert!(settings.shard_particle.click_lifetime_max_ms <= 10_000.0);
        assert!(settings.click_particle.rings_radius_min <= 2000.0);
        assert!(settings.click_particle.rings_radius_max <= 2000.0);
        assert!(settings.click_particle.rings_angular_velocity_multiplier <= 100.0);
        assert!(settings.trail_length <= 33.3);
        assert!(settings.visual.global_scale <= 64.0);
        // 下限同样是防御：0 寿命会让粒子在生成的同一帧消失，配置钳到 1ms。
        let zero = settings_from_config(&config(
            r#"
            cursor-effect {
                disk {
                    lifetime 0
                }
                rings {
                    lifetime 0
                }
            }
            "#,
        ));
        assert!(zero.click_particle.disk_lifetime_ms >= 1.0);
        assert!(zero.click_particle.rings_lifetime_ms >= 1.0);
    }

    /// N22/L1：门控纯函数必须覆盖完整的 7 布尔矩阵。
    #[test]
    fn effect_gating_matrix() {
        for bits in 0u32..(1 << 7) {
            let enabled = bits & 1 != 0;
            let hide = bits & 2 != 0;
            let desktop = bits & 4 != 0;
            let fullscreen = bits & 8 != 0;
            let window = bits & 16 != 0;
            let locked = bits & 32 != 0;
            let output = bits & 64 != 0;
            let got =
                effect_should_render(enabled, hide, desktop, fullscreen, window, locked, output);
            let want = enabled && !locked && output && !(hide && fullscreen) && (window || desktop);
            assert_eq!(got, want, "bits={bits:07b}");
        }
    }

    /// 旧写法仍然生效：`click-trigger right` + 关掉中键。
    #[test]
    fn legacy_click_trigger_still_works() {
        let settings = settings_from_config(&config(
            r#"
            cursor-effect {
                click-trigger "right"
                enable-middle-click-trigger false
            }
            "#,
        ));
        assert!(settings.click_trigger.accepts(TriggerButton::Right));
        assert!(!settings.click_trigger.accepts(TriggerButton::Left));
        assert!(!settings.click_trigger.accepts(TriggerButton::Middle));
    }
}
