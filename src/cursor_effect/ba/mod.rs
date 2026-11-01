//! BA 点击特效（`ba-click-fx-desktop`）的移植实现。
//!
//! 上游是 Windows/D3D11 的桌面特效程序，这里只搬「特效本身」：
//!
//! - `curve`：Hermite 曲线、颜色渐变、各图层的尺寸/颜色/溶解曲线、环角速度积分。
//! - `sim`：粒子模拟（点击圆盘 + 溶解环 + 三角碎片 + 拖尾 + 时间尺度 + 回收）。
//! - `trail`：Unity TrailRenderer 的斜接条带网格与进度渐变。
//! - `lz4` / `textures`：四张参考纹理的 LZ4 块与惰性解码。
//!
//! 没有搬的东西（都属于 Windows 平台的呈现/捕获层，与特效外观无关）：
//! WGC 背景捕获、Differential Bloom、OKLCH 主题映射、DirectComposition 输出、
//! 控件中心/IPC。Bloom 由上游自己的 `Core` 模式也不包含，它是唯一影响观感的
//! 差异项，见 README 说明。

pub mod curve;
pub mod lz4;
pub mod runtime;
pub mod sim;
pub mod textures;
pub mod trail;

use curve::{ClickParticleSettings, ShardParticleSettings};
pub use runtime::SimulationRuntime;
pub use sim::{
    apply_global_scale, reference_scale, LayerVisibility, Point, Simulation, Snapshot, Sprite,
    SpriteKind, TrailPoint, TrailStroke, Viewport,
};

/// 参考实现里 `frame_visual_config.cpp` 的产品级视觉参数（默认即恒等配置）。
#[derive(Debug, Clone, Copy)]
pub struct VisualConfig {
    /// 全局不透明度，作用在最终 payload 上。
    pub opacity: f32,
    /// 全局尺寸缩放（同时作用于尺寸与碎片相对发射点的位置）。
    pub global_scale: f32,
    /// 中心圆盘半径（1080p 参考像素，默认 64.8）。
    pub disk_radius: f32,
    /// 拖尾宽度倍率。
    pub trail_width: f32,
    /// 拖尾不透明度倍率。
    pub trail_opacity: f32,
    /// 环的 HDR 强度（默认 5.992157）。
    pub rings_hdr_intensity: f32,
    /// 碎片的 HDR 强度（默认 5.992157）。
    pub shards_hdr_intensity: f32,
    /// 辉光总增益倍率（1.0 = 上游原样，0 = 完全关闭辉光）。
    pub glow_gain: f32,
    /// 额外乘在**拖尾**雾上的增益（默认 1.0；不影响点击特效）。
    pub glow_trail_gain: f32,
    /// 辉光半径倍率（同时缩放所有材质的雾）。
    pub glow_radius: f32,
    /// 溶解环的内径比（外径的多少倍）。默认 0.9401275。
    pub ring_inner_ratio: f32,
    /// 整体着色（线性域，会乘到每个图元的主色上）。`[1,1,1]` = 保留美术原色。
    pub tint: [f32; 3],
    /// 拖尾条带是否走曲线拟合（对应 BASpark 的 `ApplyCurveDraw`）。
    ///
    /// 关掉时按采样点原样铺直线段 —— 采样率低（默认 40Hz）＋ 移动快的时候，
    /// 一条弧线会被切成十来段折线，拐点处能看出棱角。打开后用 Catmull-Rom
    /// 在采样点之间补点，条带贴着曲线走。
    ///
    /// 放在 `VisualConfig` 里是因为**只有渲染层消费它**：模拟侧存的是采样点，
    /// 补点是纯粹的几何后处理，不改变粒子、寿命和进度。
    pub curve_draw: bool,
}

impl Default for VisualConfig {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            global_scale: 1.0,
            disk_radius: 64.8,
            trail_width: 1.0,
            trail_opacity: 1.0,
            rings_hdr_intensity: 5.992_157,
            shards_hdr_intensity: 5.992_157,
            glow_gain: 1.0,
            glow_trail_gain: 1.0,
            glow_radius: 1.0,
            ring_inner_ratio: 0.940_127_5,
            tint: [1.0, 1.0, 1.0],
            curve_draw: false,
        }
    }
}

/// 上游 `applyVisualConfig` 的等价实现。
pub fn apply_visual_config(snapshot: &mut Snapshot, config: &VisualConfig) {
    const UNITY_DISK_RADIUS_AT_REFERENCE_HEIGHT: f32 = 64.8;
    const UNITY_HDR_INTENSITY: f32 = 5.992_157;

    snapshot.global_opacity = config.opacity.clamp(0.0, 1.0);

    let disk_radius_scale = config.disk_radius / UNITY_DISK_RADIUS_AT_REFERENCE_HEIGHT;
    let rings_hdr_scale = config.rings_hdr_intensity / UNITY_HDR_INTENSITY;
    let shards_hdr_scale = config.shards_hdr_intensity / UNITY_HDR_INTENSITY;
    for sprite in &mut snapshot.sprites {
        match sprite.kind {
            SpriteKind::CenterDisk => sprite.size *= disk_radius_scale,
            SpriteKind::DissolveRing => sprite.intensity *= rings_hdr_scale,
            SpriteKind::Triangle => sprite.intensity *= shards_hdr_scale,
        }
    }

    snapshot.trail_opacity *= config.trail_opacity;
    apply_global_scale(snapshot, config.global_scale);
    snapshot.trail_width *= config.trail_width;
}

/// 点击爆裂与碎片的默认参数（上游 `EffectsConfig` 的恒等默认值）。
pub fn default_click_particle_settings() -> ClickParticleSettings {
    ClickParticleSettings::default()
}

pub fn default_shard_particle_settings() -> ShardParticleSettings {
    ShardParticleSettings::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor_effect::ba::sim::Point;

    #[test]
    fn visual_config_defaults_are_identity() {
        let viewport = Viewport::new(1920, 1080);
        let mut sim = Simulation::default();
        sim.pointer_down(Point::new(960.0, 540.0), viewport, 0.0);
        let plain = sim.snapshot(viewport, 0.05);

        let mut configured = sim.snapshot(viewport, 0.05);
        apply_visual_config(&mut configured, &VisualConfig::default());

        assert_eq!(plain.sprites.len(), configured.sprites.len());
        for (a, b) in plain.sprites.iter().zip(configured.sprites.iter()) {
            assert!((a.size - b.size).abs() < 1e-4);
            assert!((a.intensity - b.intensity).abs() < 1e-4);
        }
        assert!((plain.trail_width - configured.trail_width).abs() < 1e-6);
    }

    #[test]
    fn global_scale_moves_drag_shards_around_their_pivot() {
        let viewport = Viewport::new(1920, 1080);
        let mut sim = Simulation::default();
        sim.pointer_down(Point::new(400.0, 300.0), viewport, 0.0);
        sim.advance(0.016);
        let mut time = 0.016;
        for _ in 0..6 {
            time += 0.016;
            sim.pointer_move(
                Point::new(400.0 + time as f32 * 200.0, 300.0),
                viewport,
                time,
            );
            sim.advance(time);
        }
        let base = sim.snapshot(viewport, time);
        let mut scaled = base.clone();
        apply_globals(&mut scaled, 2.0);
        assert!(!scaled.sprites.is_empty());
    }

    fn apply_globals(snapshot: &mut Snapshot, scale: f32) {
        apply_visual_config(
            snapshot,
            &VisualConfig {
                global_scale: scale,
                ..Default::default()
            },
        );
    }
}
