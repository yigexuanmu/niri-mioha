//! 光标特效（Cursor Effects）模块。
//!
//! 跟随鼠标的矢量粒子特效（点击爆裂 + 拖尾 + 滚轮代码圆环），
//! 在 niri 合成器内部用 GLES 绘制。

pub mod glyphs;
pub mod render;
pub mod state;

use std::time::Instant;

use niri_config::misc::{ClickTriggerType, CursorEffect as CursorEffectConfig};
pub use state::CursorEffectState;

use crate::niri::Niri;

impl Niri {
    /// 把 `cursor-effect` 配置项灌进 `CursorEffectState` 条目。
    ///
    /// 在 `Niri::new`（首次）与 `State::reload_config`（重载）处调用，
    /// 按字段语义同步；**不触碰 `enabled`**——`enabled` 由运行时 toggle 承载，
    /// 配置项只是初始值（首次初始化在 `Niri::new` 末尾单独设置一次）。
    pub fn apply_cursor_effect_config(&mut self, cfg: &CursorEffectConfig) {
        let ce = &mut self.cursor_effect;
        ce.persistent_trail = cfg.enable_always_trail;
        ce.apply_curve_draw = cfg.apply_curve_draw;
        ce.touch_mode = cfg.touch_mode;
        ce.middle_click_trigger = cfg.enable_middle_click_trigger;
        ce.hide_in_fullscreen = cfg.hide_in_fullscreen;
        ce.show_on_desktop = cfg.show_on_desktop;
        ce.scale = cfg.scale as f32;
        ce.opacity = cfg.opacity as f32;
        // 拖尾按 ~2px 密集采样（每事件 + 段内细分）；点容量提高后快速拖动
        // 不再被截断 → 拖尾长度随拖动速度变长（点按距离采样、按帧衰减）。
        ce.max_trail = 1200;
        ce.trail_refresh_hz = cfg.trail_refresh_rate;
        ce.color = cfg.color.0;
        ce.color_left = cfg.color.0;
        ce.rings_end_color = state::rings_end_color_from_rgb(cfg.color.0);

        ce.click_trigger = match cfg.click_trigger {
            ClickTriggerType::Left => state::ClickTrigger::Left,
            ClickTriggerType::Right => state::ClickTrigger::Right,
            ClickTriggerType::Both => state::ClickTrigger::Both,
        };

        // use_linked 时 trail/click 都取 effect_speed。
        let (trail_speed, click_speed) = if cfg.use_linked_animation_speed {
            (cfg.effect_speed, cfg.effect_speed)
        } else {
            (cfg.trail_speed, cfg.click_speed)
        };
        ce.trail_speed = trail_speed as f32;
        ce.click_speed = click_speed as f32;
    }

    /// 每帧推进光标特效状态机；返回 `true` 表示仍有活动粒子，调用方据此决定
    /// 是否即便无其它 damage 也提交一帧。
    pub fn advance_cursor_effect(&mut self, now: Instant) -> bool {
        self.cursor_effect.advance(now)
    }
}
