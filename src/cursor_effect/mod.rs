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

use niri_config::misc::{
    ClickTriggerButton, ClickTriggerType, CursorEffect as CursorEffectConfig,
};
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
    /// - `color` 被忽略：上游这个字段在 niri 侧没有对应实现，着色请用 `tint`。
    /// - `trail-refresh-rate` → 拖尾的输入采样率（上游 `inputSamplingRateHz`）：> 0
///   时按该频率采样指针事件，0 表示每个事件都采。
/// - `apply-curve-draw` → 拖尾条带是否用 Catmull-Rom 补点（见 `VisualConfig::curve_draw`）。
/// - `touch-mode` → 是否让触摸输入也驱动特效（见 `CursorEffectState::touch_mode`）。
    pub fn apply_cursor_effect_config(&mut self, cfg: &CursorEffectConfig) {
        self.cursor_effect.apply_settings(settings_from_config(cfg));
    }

    /// 每帧推进光标特效状态机；返回 `true` 表示仍有活动粒子，调用方据此决定
    /// 是否即便无其它 damage 也提交一帧。
    pub fn advance_cursor_effect(&mut self, now: Instant) -> bool {
        let alive = self.cursor_effect.advance(now);
        self.cursor_effect.on_frame_rendered(now);
        alive
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
        return ClickTrigger::from_buttons(
            buttons
                .iter()
                .map(|b| trigger_button_from_config(*b)),
        );
    }
    let mut trigger = match cfg.click_trigger {
        ClickTriggerType::Left => ClickTrigger::single(TriggerButton::Left),
        ClickTriggerType::Right => ClickTrigger::single(TriggerButton::Right),
        ClickTriggerType::Both => ClickTrigger::from_buttons([
            TriggerButton::Left,
            TriggerButton::Right,
        ]),
    };
    if cfg.enable_middle_click_trigger {
        trigger = trigger.with(TriggerButton::Middle);
    }
    trigger
}

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
            opacity: cfg.opacity as f32,
            global_scale: cfg.scale as f32,
            disk_radius: cfg.disk.radius as f32,
            trail_width: cfg.trail.width as f32,
            trail_opacity: cfg.trail.opacity as f32,
            rings_hdr_intensity: cfg.rings.hdr_intensity as f32,
            shards_hdr_intensity: cfg.shards.hdr_intensity as f32,
            glow_gain: cfg.glow.gain as f32,
            glow_trail_gain: cfg.glow.trail_gain as f32,
            glow_radius: cfg.glow.radius as f32,
            ring_inner_ratio: cfg.rings.inner_ratio as f32,
            tint: [
                cfg.tint.0[0] as f32 / 255.0,
                cfg.tint.0[1] as f32 / 255.0,
                cfg.tint.0[2] as f32 / 255.0,
            ],
            curve_draw: cfg.apply_curve_draw,
        },
        click_particle: ba::curve::ClickParticleSettings {
            disk_lifetime_ms: cfg.disk.lifetime as f32,
            rings_count: cfg.rings.count,
            rings_lifetime_ms: cfg.rings.lifetime as f32,
            rings_radius_min: cfg.rings.radius_min as f32,
            rings_radius_max: cfg.rings.radius_max as f32,
            rings_angular_velocity_multiplier: cfg.rings.spin as f32,
            rings_rotation_direction: cfg.rings.spin_direction as f32,
        },
        shard_particle: ba::curve::ShardParticleSettings {
            click_count: cfg.shards.count,
            click_lifetime_min_ms: cfg.shards.lifetime_min as f32,
            click_lifetime_max_ms: cfg.shards.lifetime_max as f32,
            click_radius: cfg.shards.radius as f32,
            click_speed_min: cfg.shards.speed_min as f32,
            click_speed_max: cfg.shards.speed_max as f32,
            size_min: cfg.shards.size_min as f32,
            size_max: cfg.shards.size_max as f32,
        },
        click_trigger: click_trigger_from_config(cfg),
        persistent_trail: cfg.enable_always_trail,
        hide_in_fullscreen: cfg.hide_in_fullscreen,
        show_on_desktop: cfg.show_on_desktop,
        trail_length: cfg.trail.length as f32,
        click_time_scale: click_speed as f32,
        trail_time_scale: trail_speed as f32,
        trail_refresh_rate: cfg.trail_refresh_rate,
        touch_mode: cfg.touch_mode,
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
        assert_eq!(default.trail_refresh_rate, 40);
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
        // 不写 → 上游默认（左键 + 中键）。
        let default = settings_from_config(&config("cursor-effect {}"));
        assert!(default.click_trigger.accepts(TriggerButton::Left));
        assert!(default.click_trigger.accepts(TriggerButton::Middle));
        assert!(!default.click_trigger.accepts(TriggerButton::Right));
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
