//! 光标特效（Cursor Effects）模块。
//!
//! 点击爆裂 + 拖动拖尾，移植自
//! [`CialloKing/ba-click-fx-desktop`](https://github.com/CialloKing/ba-click-fx-desktop)
//! （《蔚蓝档案》的 FX_Touch 点击特效重建）。粒子模拟、曲线、纹理都来自上游，
//! 这里只负责接上 niri 的指针事件与 GLES 渲染。

pub mod ba;
pub mod render;
pub mod state;

use std::time::Instant;

use niri_config::misc::{ClickTriggerType, CursorEffect as CursorEffectConfig};
use smithay::utils::{Logical, Point, Size};

use self::ba::{Point as FxPoint, Viewport};
pub use state::{ClickTrigger, CursorEffectSettings, CursorEffectState};

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

impl Niri {
    /// 把 `cursor-effect` 配置翻译成模拟参数。
    ///
    /// 映射说明（旧配置字段继续沿用，不新增语法）：
    /// - `enabled` → 模拟总开关；`opacity` → 全局不透明度。
    /// - `scale` → 全局尺寸缩放，**1.0 才是上游原尺寸**（旧默认 1.5 会放大 1.5 倍）。
    /// - `effect-speed`（或 `use-linked-animation-speed false` 时的
    ///   `trail-speed` / `click-speed`）→ 点击/拖尾时间尺度。
    /// - `enable-always-trail` → 不按键也画拖尾；`click-trigger` /
    ///   `enable-middle-click-trigger` → 哪些按键触发爆裂。
    /// - `hide-in-fullscreen` / `show-on-desktop` → 环境过滤。
    /// - `color` 被忽略：BA 特效的配色是美术固化的（白→蓝渐变），
    ///   套一层自定义色就不再是 1:1 了。
    /// - `trail-refresh-rate` / `apply-curve-draw` / `touch-mode` 无对应实现。
    pub fn apply_cursor_effect_config(&mut self, cfg: &CursorEffectConfig) {
        let (trail_speed, click_speed) = if cfg.use_linked_animation_speed {
            (cfg.effect_speed, cfg.effect_speed)
        } else {
            (cfg.trail_speed, cfg.click_speed)
        };

        let settings = CursorEffectSettings {
            enabled: cfg.enabled,
            visual: ba::VisualConfig {
                opacity: cfg.opacity as f32,
                global_scale: cfg.scale as f32,
                ..Default::default()
            },
            click_particle: ba::curve::ClickParticleSettings::default(),
            shard_particle: ba::curve::ShardParticleSettings::default(),
            click_trigger: match cfg.click_trigger {
                ClickTriggerType::Left => ClickTrigger::Left,
                ClickTriggerType::Right => ClickTrigger::Right,
                ClickTriggerType::Both => ClickTrigger::Both,
            },
            middle_click_trigger: cfg.enable_middle_click_trigger,
            persistent_trail: cfg.enable_always_trail,
            hide_in_fullscreen: cfg.hide_in_fullscreen,
            show_on_desktop: cfg.show_on_desktop,
            trail_length: 1.0,
            click_time_scale: click_speed as f32,
            trail_time_scale: trail_speed as f32,
        };
        self.cursor_effect.apply_settings(settings);
    }

    /// 每帧推进光标特效状态机；返回 `true` 表示仍有活动粒子，调用方据此决定
    /// 是否即便无其它 damage 也提交一帧。
    pub fn advance_cursor_effect(&mut self, now: Instant) -> bool {
        let alive = self.cursor_effect.advance(now);
        self.cursor_effect.on_frame_rendered(now);
        alive
    }
}
