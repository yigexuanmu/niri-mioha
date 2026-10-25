//! 光标特效 GLES 渲染层（Cursor Effects Rendering）。
//!
//! 分两层：
//!
//! 1. [`build_commands`]：把一帧 `Snapshot` 翻成纯数据（[`DrawCommand`]）——四边
//!    形的屏幕矩形、材质种类、该材质需要的全部参数。不含任何 GL 对象，因此可以
//!    离线跑（单元测试、以及和上游 C++ 参考实现比对）。
//! 2. [`collect_render_elements`]：把 `DrawCommand` 包成 `ShaderRenderElement`
//!    （与 `BorderRenderElement` 同模板），绑定 uniform 与纹理。
//!
//! 四类材质与上游 `fxMaterialsShaderSource` 一一对应：
//!
//! - 中心圆盘（`Cross`）—— 源覆盖，会压暗背景；
//! - 溶解环（`Dissolve`）—— 极坐标重建 `Cylinder002` 的 uv；
//! - 三角碎片（`Additive`）—— 图集左右两帧；
//! - 拖尾（`Trail`）—— 逐段贴一条条带，段内沿进度插值颜色与纵向覆盖。
//!
//! 几何约定：四边形永远是轴对齐的，`input_to_geo` 只把 `niri_v_coords ∈ [0,1]²`
//! 放大成「相对四边形左上角的物理像素」。精灵用 `u_center`/`u_rot`/`u_size` 在
//! 着色器里反旋并铺满方形；拖尾段用 `u_origin`/`u_axis`/`u_extent` 把像素投影到
//! 条带坐标系（段外不发光），于是也只需要一张轴对齐的包围盒四边形。
//!
//! 四边形还额外留白：辉光是解析求的、会溢出形状边缘，四边形不够大就会在边界
//! 上把光晕切出一圈台阶。精灵按 [`BLOOM_QUAD_SCALE`] 放大；拖尾按屏幕空间的
//! [`BLOOM_TRAIL_HALO_PX`]（1080p 参考像素 × 输出倍率）外扩。
//!
//! 混合：上游把 direct 与 bloom 在线性域相加、`LinearToSrgb` 编码、再按
//! `capacityScale` 压成预乘色，最后用源覆盖交给系统合成。这里复刻同一套数学，
//! 因此**全部材质**都用 smithay 默认的 `ONE, ONE_MINUS_SRC_ALPHA`，不需要像旧
//! 的字符粒子那样临时切加法混合。唯一的差别在 bloom：上游是独立的 mip 链后处理，
//! niri 的「一个元素一次 draw」管线里没有后处理挂点，所以在 fragment shader 里用
//! 解析形状近似（见 `cursor_effect.frag` 的辉光一节）。
//!
//! 注意辉光半径是**屏幕空间**量（像素），不是图元尺寸的倍数。所以拖尾的留白与
//! 光晕衰减都走 [`ba::reference_scale`]，绝不能拿 smithay 的 `niri_size`（那是
//! 四边形自身的尺寸）去归一化。

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use glam::{Mat3, Vec2};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::ImportMem as _;
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::cursor_effect::ba;
use crate::cursor_effect::state::CursorEffectState;
use crate::render_helpers::renderer::{AsGlesFrame as _, NiriRenderer};
use crate::render_helpers::shader_element::ShaderRenderElement;
use crate::render_helpers::shaders::{mat3_uniform, ProgramType, Shaders};

/// 上游 `fx_gpu_renderer.cpp` 的 `trailArtisticIntensity`。
const TRAIL_ARTISTIC_INTENSITY: f32 = 23.968_628;

/// 精灵/拖尾段的四边形额外留白（物理像素），避免柔边被矩形裁掉。
const QUAD_PADDING: f64 = 1.0;

/// 四边形相对图元的放大倍数，给辉光留出溢出空间。
///
/// 着色器里的辉光按 `BLOOM_SIGMA = 0.4`（单位：图元半尺寸）扩散，在 2σ 处截断，
/// 即边缘外还要 0.8 个半尺寸，所以四边形取 2.4 倍全长留足余量 —— 光晕若被四边形
/// 裁到，边界上会出现一圈台阶。
const BLOOM_QUAD_SCALE: f64 = 2.4;

/// 与 `cursor_effect.frag` 的 `BLOOM_SIGMA` 保持同步（单位：图元半尺寸）。
/// 只在测试里用于校验留白是否装得下光晕，因此不参与运行时计算。
#[allow(dead_code)]
const BLOOM_SIGMA: f64 = 0.4;

/// 与 `cursor_effect.frag` 的 `BLOOM_TRAIL_HALO_END` 保持同步（1080p 参考像素）。
///
/// 光晕是屏幕空间的：半径只跟输出尺寸有关，与实际带宽无关。所以留白是「110 个
/// 1080p 像素 × 输出倍率」，不再按带宽的倍数给。
const BLOOM_TRAIL_HALO_PX: f64 = 110.0;

/// 圆盘外晕的屏幕空间半径（1080p 参考像素），与 `cursor_effect.frag` 的
/// `DISK_HALO_END` 保持同步。
///
/// 圆盘是点击特效的视觉锚点：官方图里那圈蔚蓝的雾一直铺到盘边外 ~130px，靠
/// `BLOOM_QUAD_SCALE` 那个「按图元倍数」的留白远远撑不到 —— 圆盘本身只有几十
/// 像素，乘 2.4 还是几十像素，雾会被四边形切成一个方块。所以这里和拖尾一样按
/// 屏幕像素给。
const DISK_HALO_PX: f64 = 160.0;

/// 辉光增益，按上游官方演示图（`docs/images/native-click-trail.png`）的径向剖面
/// 标定：圆盘内部约 +25%，1.6 倍半径处衰减到 ~5%，与实测 (0.31,0.35,0.50) 吻合。
const BLOOM_GAIN: f32 = 0.25;

/// 材质种类，值与 `cursor_effect.frag` 的 `u_mode` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Material {
    /// 中心圆盘（`CrossPixel`）。
    Disk = 0,
    /// 溶解环（`DissolvePixel`）。
    Ring = 1,
    /// 三角碎片（`AdditivePixel`）。
    Triangle = 2,
    /// 拖尾（`TrailPixel`）。
    Trail = 3,
}

impl Material {
    fn texture_name(self) -> &'static str {
        match self {
            Self::Disk => "tex_disk",
            Self::Ring => "tex_ring",
            Self::Triangle => "tex_triangle",
            Self::Trail => "tex_trail",
        }
    }
}

/// 一个待绘制的图元（纯数据）。四边形是输出局部物理像素坐标。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawCommand {
    /// 轴对齐四边形（输出局部物理像素）。
    pub quad: Rectangle<f64, Physical>,
    pub material: Material,
    /// 主色（线性域）。
    pub color: [f32; 3],
    /// 拖尾：段尾颜色（线性域）。
    pub color_b: [f32; 3],
    /// 顶点色 alpha。
    pub color_a: f32,
    /// 全局不透明度。
    pub gop: f32,
    /// Unity HDR 强度。
    pub intensity: f32,
    /// 溶解裁剪阈值。
    pub threshold: f32,
    /// 精灵中心（相对四边形左上角的像素）。
    pub center: [f32; 2],
    /// 精灵边长（像素）。
    pub size: f32,
    /// 精灵/环旋转（弧度）。
    pub rot: f32,
    /// 图集 u 起点与跨度。
    pub uv_off: [f32; 2],
    /// 拖尾：两端纵向覆盖。
    pub cov: [f32; 2],
    /// 拖尾：纹理 u 起点与增量。
    pub progress: [f32; 2],
    /// 拖尾：切线 xy、法线 zw。
    pub axis: [f32; 4],
    /// 拖尾：段起点（相对四边形左上角的像素）。
    pub origin: [f32; 2],
    /// 拖尾：段长与带宽（像素）。
    pub extent: [f32; 2],
    /// 拖尾：首段 / 尾段标记（1 = 辉光在它这一端收成圆头）。
    pub cap: [f32; 2],
    /// 输出高度相对 1080p 的倍率（辉光半径的像素口径）。
    pub ref_scale: f32,
}

impl DrawCommand {
    /// 光晕的屏幕空间外扩（物理像素）。辉光半径只跟输出尺寸有关，跟图元大小
    /// 无关，所以这里按 1080p 参考像素 × 输出倍率给，而不是按图元的倍数。
    fn halo_pad(material: Material, base: &Base) -> f64 {
        let scale = if base.ref_scale.is_finite() && base.ref_scale > 0.0 {
            base.ref_scale as f64
        } else {
            1.0
        };
        match material {
            Material::Disk => DISK_HALO_PX * scale,
            // 环 / 碎片的辉光仍在图元自身的尺度上（`BLOOM_SIGMA`），而且它们
            // 数量多、尺寸小，按屏幕像素外扩会把四边形撑得过大。
            Material::Ring | Material::Triangle | Material::Trail => 0.0,
        }
    }

    fn sprite(
        material: Material,
        center: ba::Point,
        size: f32,
        rot: f32,
        rotates: bool,
        uv_off: [f32; 2],
        base: &Base,
    ) -> Self {
        let half = size.max(0.0) as f64 * 0.5;
        let half = if rotates {
            half * (rot.sin().abs() as f64 + rot.cos().abs() as f64)
        } else {
            half
        } * BLOOM_QUAD_SCALE
            + QUAD_PADDING
            + Self::halo_pad(material, base);
        let side = half * 2.0;
        let quad = Rectangle::new(
            Point::from((center.x as f64 - half, center.y as f64 - half)),
            Size::from((side, side)),
        );
        Self {
            quad,
            material,
            color: base.color,
            color_b: base.color,
            color_a: base.color_a,
            gop: base.gop,
            intensity: base.intensity,
            threshold: base.threshold,
            center: [side as f32 * 0.5, side as f32 * 0.5],
            size,
            rot,
            uv_off,
            cov: [1.0, 1.0],
            progress: [0.0, 1.0],
            axis: [1.0, 0.0, 0.0, 1.0],
            origin: [0.0, 0.0],
            extent: [side as f32, side as f32],
            cap: [0.0, 0.0],
            ref_scale: base.ref_scale,
        }
    }
}

/// 一帧里各图元共用的量。
struct Base {
    color: [f32; 3],
    color_a: f32,
    gop: f32,
    intensity: f32,
    threshold: f32,
    ref_scale: f32,
}

/// 把一帧快照翻成绘制命令列表（纯函数，按绘制顺序排列）。
pub fn build_commands(snapshot: &ba::Snapshot) -> Vec<DrawCommand> {
    let mut out = Vec::with_capacity(64);

    // 拖尾先画（上游把拖尾当作粒子底下的连续笔画）。
    build_trail(snapshot, &mut out);

    let mut sprites: Vec<&ba::Sprite> = snapshot.sprites.iter().collect();
    sprites.sort_by_key(|sprite| sprite.render_queue);
    for sprite in sprites {
        if !(sprite.size > 0.0) || !sprite.size.is_finite() {
            continue;
        }
        let base = Base {
            color: sprite.color.to_array(),
            color_a: sprite.color.a,
            gop: snapshot.global_opacity,
            intensity: sprite.intensity,
            threshold: sprite.dissolve_threshold,
            ref_scale: snapshot.ref_scale,
        };
        let command = match sprite.kind {
            ba::SpriteKind::CenterDisk => DrawCommand::sprite(
                Material::Disk,
                sprite.center,
                sprite.size,
                sprite.rotation,
                true,
                [0.0, 1.0],
                &base,
            ),
            ba::SpriteKind::DissolveRing => DrawCommand::sprite(
                Material::Ring,
                sprite.center,
                sprite.size,
                sprite.rotation,
                false,
                [0.0, 1.0],
                &base,
            ),
            ba::SpriteKind::Triangle => DrawCommand::sprite(
                Material::Triangle,
                sprite.center,
                sprite.size,
                sprite.rotation,
                true,
                // 图集左右两帧各占半个 u。
                [if sprite.atlas_frame == 0 { 0.0 } else { 0.5 }, 0.5],
                &base,
            ),
        };
        out.push(command);
    }

    out
}

/// 拖尾：把相邻两个拖尾点之间贴一条四边形，逐顶点插值渐变颜色与纵向覆盖。
///
/// 多个点击实例同时存在时会有多条独立笔画，逐条单独铺段 —— 跨笔画接起来会画出
/// 一条并不存在的连线。
fn build_trail(snapshot: &ba::Snapshot, out: &mut Vec<DrawCommand>) {
    for (points, width) in snapshot.strokes() {
        build_trail_stroke(points, width, snapshot, out);
    }
}

fn build_trail_stroke(
    points: &[ba::TrailPoint],
    width: f32,
    snapshot: &ba::Snapshot,
    out: &mut Vec<DrawCommand>,
) {
    if points.len() < 2 || !(width > 0.0) || !width.is_finite() {
        return;
    }
    let ref_scale = if snapshot.ref_scale.is_finite() && snapshot.ref_scale > 0.0 {
        snapshot.ref_scale
    } else {
        1.0
    };

    // 累计弧长 → 归一化进度（上游 `makeUnityTrailMesh` 用同一套进度）。
    let mut distances = Vec::with_capacity(points.len());
    distances.push(0.0f32);
    for pair in points.windows(2) {
        let dx = pair[1].position.x - pair[0].position.x;
        let dy = pair[1].position.y - pair[0].position.y;
        let last = *distances.last().unwrap();
        distances.push(last + (dx * dx + dy * dy).sqrt());
    }
    let total = *distances.last().unwrap();
    if !(total > 1e-3) || !total.is_finite() {
        return;
    }

    let half_width = width * 0.5;
    for index in 1..points.len() {
        let from = points[index - 1].position;
        let to = points[index].position;
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        let length = (dx * dx + dy * dy).sqrt();
        if !length.is_finite() || length < 1e-3 {
            continue;
        }
        let tangent = (dx / length, dy / length);
        let normal = (-tangent.1, tangent.0);

        // 四个角 → 轴对齐包围盒；再向外留辉光的溢出空间。
        let (nx, ny) = (normal.0 * half_width, normal.1 * half_width);
        let halo_pad = BLOOM_TRAIL_HALO_PX * ref_scale as f64;
        let pad = halo_pad + QUAD_PADDING;
        let min_x = (from.x - nx).min(to.x - nx) as f64 - pad;
        let max_x = (from.x + nx).max(to.x + nx) as f64 + pad;
        let min_y = (from.y - ny).min(to.y - ny) as f64 - pad;
        let max_y = (from.y + ny).max(to.y + ny) as f64 + pad;
        let quad = Rectangle::new(
            Point::from((min_x, min_y)),
            Size::from((max_x - min_x, max_y - min_y)),
        );

        let from_progress = distances[index - 1] / total;
        let to_progress = distances[index] / total;
        out.push(DrawCommand {
            quad,
            material: Material::Trail,
            color: ba::trail::evaluate_unity_trail_color(from_progress),
            color_b: ba::trail::evaluate_unity_trail_color(to_progress),
            color_a: snapshot.trail_opacity,
            gop: snapshot.global_opacity,
            intensity: TRAIL_ARTISTIC_INTENSITY,
            threshold: 0.0,
            center: [0.0, 0.0],
            size: width,
            rot: 0.0,
            uv_off: [0.0, 1.0],
            cov: [
                ba::trail::trail_longitudinal_coverage(from_progress),
                ba::trail::trail_longitudinal_coverage(to_progress),
            ],
            // 上游 uv.x = 1 - progress，沿段线性推进。
            progress: [
                1.0 - from_progress,
                -(to_progress - from_progress),
            ],
            axis: [tangent.0, tangent.1, normal.0, normal.1],
            origin: [from.x - quad.loc.x as f32, from.y - quad.loc.y as f32],
            extent: [length, width],
            // 首/尾段标记：辉光在笔画两端收成圆头（见 shader 的 `u_cap`），
            // 中间的段保持硬切，相邻两段加起来才是 1。
            cap: [
                if index == 1 { 1.0 } else { 0.0 },
                if index + 1 == points.len() { 1.0 } else { 0.0 },
            ],
            ref_scale,
        });
    }
}

/// 四张材质纹理，四类材质各用一张。
#[derive(Debug, Clone)]
pub struct CursorEffectTextures {
    pub disk: GlesTexture,
    pub ring: GlesTexture,
    pub triangle: GlesTexture,
    pub trail: GlesTexture,
}

impl CursorEffectTextures {
    fn get(&self, material: Material) -> &GlesTexture {
        match material {
            Material::Disk => &self.disk,
            Material::Ring => &self.ring,
            Material::Triangle => &self.triangle,
            Material::Trail => &self.trail,
        }
    }
}

/// 把四张内嵌的 LZ4 纹理解码并上传。四张必须同时可用，否则整个特效应关闭。
pub fn upload_textures(renderer: &mut GlesRenderer) -> Result<CursorEffectTextures, GlesError> {
    fn upload(
        renderer: &mut GlesRenderer,
        texture: &ba::textures::FxTexture,
    ) -> Result<GlesTexture, GlesError> {
        // 上游按 `R8G8B8A8_UNORM_SRGB` 采样，也就是逐通道做 sRGB 解码；这里上传
        // 原始 RGBA8 字节（`Abgr8888` 在 GL 侧就是 RGBA + UNSIGNED_BYTE），解码
        // 交给 `cursor_effect.frag` 里的 `srgb_to_linear()`。
        renderer.import_memory(
            &texture.rgba,
            Fourcc::Abgr8888,
            Size::from((texture.width as i32, texture.height as i32)),
            false,
        )
    }

    Ok(CursorEffectTextures {
        disk: upload(renderer, ba::textures::center_disk())?,
        ring: upload(renderer, ba::textures::dissolve_ring())?,
        triangle: upload(renderer, ba::textures::triangle_atlas())?,
        trail: upload(renderer, ba::textures::trail())?,
    })
}

/// 一个光标特效图元的渲染元素（与 `BorderRenderElement` 同模板）。
#[derive(Debug, Clone)]
pub struct CursorEffectElement {
    inner: ShaderRenderElement,
}

impl CursorEffectElement {
    /// 组装一个轴对齐四边形并绑定 uniform / 纹理。`scale` 用来把物理像素换算回
    /// 逻辑坐标（smithay 的元素尺寸是逻辑像素）。
    fn build(
        command: &DrawCommand,
        scale: f32,
        texture: &GlesTexture,
    ) -> Self {
        let inverse = 1.0 / scale.max(f32::EPSILON) as f64;
        let loc = Point::<f64, Logical>::from((
            command.quad.loc.x * inverse,
            command.quad.loc.y * inverse,
        ));
        let size = Size::<f64, Logical>::from((
            command.quad.size.w * inverse,
            command.quad.size.h * inverse,
        ));
        if size.w < 1e-3 || size.h < 1e-3 {
            return Self {
                inner: ShaderRenderElement::empty(ProgramType::CursorEffect, Kind::Unspecified),
            };
        }

        let input_to_geo =
            Mat3::from_scale(Vec2::new(command.quad.size.w as f32, command.quad.size.h as f32));
        let uniforms: Rc<[Uniform<'static>]> = Rc::from(vec![
            mat3_uniform("input_to_geo", input_to_geo).into_owned(),
            Uniform::new("u_mode", command.material as u32 as f32),
            Uniform::new("u_color", command.color),
            Uniform::new("u_color_b", command.color_b),
            Uniform::new("u_color_a", command.color_a),
            Uniform::new("u_gop", command.gop),
            Uniform::new("u_intensity", command.intensity),
            Uniform::new("u_threshold", command.threshold),
            Uniform::new("u_center", command.center),
            Uniform::new("u_size", command.size),
            Uniform::new("u_rot", command.rot),
            Uniform::new("u_uv_off", command.uv_off),
            Uniform::new("u_cov", command.cov),
            Uniform::new("u_progress", command.progress),
            Uniform::new("u_axis", command.axis),
            Uniform::new("u_origin", command.origin),
            Uniform::new("u_extent", command.extent),
            Uniform::new("u_cap", command.cap),
            Uniform::new("u_bloom", BLOOM_GAIN),
            Uniform::new("u_ref_scale", command.ref_scale),
        ]);

        let textures = HashMap::from([(
            command.material.texture_name().to_string(),
            texture.clone(),
        )]);
        let mut inner = ShaderRenderElement::empty(ProgramType::CursorEffect, Kind::Unspecified);
        inner.update(size, None, scale, 1.0, uniforms, textures);
        inner = inner.with_location(loc);
        Self { inner }
    }
}

impl Element for CursorEffectElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for CursorEffectElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let _span = tracy_client::span!("CursorEffectElement::draw");
        frame.with_gpu_span(gpu_span_location!("CursorEffectElement::draw"), |frame| {
            // 着色器输出预乘 sRGB，正好配 smithay 的默认 `ONE, ONE_MINUS_SRC_ALPHA`。
            RenderElement::<GlesRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            )
        })
    }
    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for CursorEffectElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(self, frame, src, dst, damage, opaque_regions, cache)?;
        Ok(())
    }
    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

/// 收集一个输出上需要绘制的光标特效元素。
///
/// `Snapshot` 里的坐标已经是「相对该输出左上角」的物理像素（模拟的视口就是输出
/// 的物理尺寸），所以这里不需要输出在全局空间里的位置。
pub fn collect_render_elements(
    renderer: &mut impl NiriRenderer,
    state: &CursorEffectState,
    now: Instant,
    scale: f32,
) -> Vec<CursorEffectElement> {
    if !state.enabled {
        tracing::debug!("cursor-effect: disabled");
        return Vec::new();
    }
    let Some(textures) = Shaders::get(renderer).cursor_effect_textures.clone() else {
        tracing::debug!("cursor-effect: textures not initialized");
        return Vec::new();
    };

    let snapshot = state.snapshot(state.viewport(), now);
    if !snapshot.has_drawable_content() {
        tracing::debug!(
            "cursor-effect: snapshot empty (sprites={} strokes={} viewport={:?})",
            snapshot.sprites.len(),
            snapshot.trail_strokes.len(),
            state.viewport()
        );
        return Vec::new();
    }

    let commands = build_commands(&snapshot);
    tracing::debug!(
        "cursor-effect: {} commands (sprites={} strokes={} ref_scale={:.3} viewport={:?})",
        commands.len(),
        snapshot.sprites.len(),
        snapshot.trail_strokes.len(),
        snapshot.ref_scale,
        state.viewport()
    );
    commands
        .iter()
        .map(|command| {
            tracing::debug!(
                "cursor-effect: {:?} quad=({},{} {}x{})",
                command.material,
                command.quad.loc.x,
                command.quad.loc.y,
                command.quad.size.w,
                command.quad.size.h
            );
            CursorEffectElement::build(command, scale, textures.get(command.material))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click_snapshot() -> ba::Snapshot {
        let viewport = ba::Viewport::new(1920, 1080);
        let mut sim = ba::Simulation::new(ba::sim::DEFAULT_SEED);
        sim.pointer_down(ba::Point::new(960.0, 540.0), viewport, 0.0);
        sim.advance(0.016);
        for step in 1..=8 {
            let time = 0.016 * step as f64;
            sim.pointer_move(
                ba::Point::new(960.0 + time as f32 * 300.0, 540.0),
                viewport,
                time,
            );
        }
        sim.advance(0.15);
        sim.snapshot(viewport, 0.15)
    }

    #[test]
    fn click_produces_disk_and_ring_before_shards() {
        let snapshot = click_snapshot();
        let commands = build_commands(&snapshot);
        assert!(commands.len() >= 2);
        assert_eq!(commands[0].material, Material::Trail);
        let disk = commands
            .iter()
            .position(|c| c.material == Material::Disk)
            .expect("圆盘");
        let ring = commands
            .iter()
            .position(|c| c.material == Material::Ring)
            .expect("圆环");
        assert!(disk < ring, "圆盘先于圆环绘制");
        // 三角形（队列 4550）永远在圆盘/圆环（4499）之后。
        if let Some(triangle) = commands.iter().position(|c| c.material == Material::Triangle) {
            assert!(triangle > ring);
        }
    }

    #[test]
    fn sprite_quad_covers_the_rotated_square() {
        let base = Base {
            color: [1.0; 3],
            color_a: 1.0,
            gop: 1.0,
            intensity: 1.0,
            threshold: 0.0,
            ref_scale: 1.0,
        };
        let command = DrawCommand::sprite(
            Material::Triangle,
            ba::Point::new(100.0, 100.0),
            40.0,
            std::f32::consts::FRAC_PI_4,
            true,
            [0.5, 0.5],
            &base,
        );
        // 45° 旋转后包围盒边长 = 40 * (sin + cos) * 辉光放大倍数 + 2 * padding。
        let expected = 40.0 * std::f32::consts::SQRT_2 as f64 * BLOOM_QUAD_SCALE
            + QUAD_PADDING * 2.0;
        assert!((command.quad.size.w - expected).abs() < 1e-3);
        assert!((command.quad.loc.x - (100.0 - expected * 0.5)).abs() < 1e-3);
        // 图集第二帧：u 起点 0.5、跨度 0.5。
        assert_eq!(command.uv_off, [0.5, 0.5]);
    }

    #[test]
    fn bloom_halo_fits_inside_the_padded_quad() {
        // 辉光在 2σ 处截断（见 `cursor_effect.frag` 的 `blurred_inside`），四边形
        // 必须比「图元半尺寸 + 2σ」更大，否则光晕会在四边形边界上留一圈硬边。
        let halo = 1.0 + 2.0 * BLOOM_SIGMA;
        assert!(
            BLOOM_QUAD_SCALE > halo,
            "四边形放大 {BLOOM_QUAD_SCALE} 倍，容不下 {halo} 倍半尺寸的辉光"
        );
    }

    #[test]
    fn disk_quad_keeps_room_for_the_screen_space_halo() {
        // 圆盘外晕同样是屏幕空间的：留白 = `DISK_HALO_PX × 输出倍率`，与盘的大小
        // 无关。盘只有几十像素，按倍数留白会被四边形切成一个方块。
        for height in [1080_u32, 1440, 2160] {
            let scale = ba::reference_scale(ba::Viewport::new(1920, height));
            let base = Base {
                color: [1.0; 3],
                color_a: 1.0,
                gop: 1.0,
                intensity: 2.4,
                threshold: 0.0,
                ref_scale: scale,
            };
            let size = 0.08 * height as f32;
            let command = DrawCommand::sprite(
                Material::Disk,
                ba::Point::new(500.0, 500.0),
                size,
                0.0,
                true,
                [0.0, 1.0],
                &base,
            );
            // 半个四边形 = 盘半尺寸 × 放大倍数 + padding + 屏幕空间留白。
            let need = size as f64 * 0.5 * BLOOM_QUAD_SCALE
                + QUAD_PADDING
                + DISK_HALO_PX * scale as f64;
            assert!(
                (command.quad.size.w * 0.5 - need).abs() < 1e-3,
                "高度 {height}：圆盘四边形 {:.1} 装不下 {need:.1} 的光晕留白",
                command.quad.size.w * 0.5
            );
        }

        // 其它图元不该跟着外扩：碎片数量多、尺寸小，按屏幕像素撑开会浪费填充率。
        let base = Base {
            color: [1.0; 3],
            color_a: 1.0,
            gop: 1.0,
            intensity: 1.0,
            threshold: 0.0,
            ref_scale: 1.0,
        };
        let triangle = DrawCommand::sprite(
            Material::Triangle,
            ba::Point::new(100.0, 100.0),
            40.0,
            0.0,
            true,
            [0.0, 0.5],
            &base,
        );
        let expected = 40.0 * BLOOM_QUAD_SCALE + QUAD_PADDING * 2.0;
        assert!((triangle.quad.size.w - expected).abs() < 1e-3);
    }

    #[test]
    fn trail_quad_keeps_room_for_the_screen_space_halo() {
        // 拖尾的光晕是屏幕空间的：留白等于「1080p 参考像素 × 输出倍率」，与带宽
        // 无关。1080p / 1440p / 4K 三档都要盖住光晕半径。
        for height in [1080_u32, 1440, 2160] {
            let scale = ba::reference_scale(ba::Viewport::new(1920, height));
            // 一条水平笔画：包围盒高度 = 带宽 + 2 × 留白，便于反解留白。
            let width = 0.005 * height as f32 / 2.0;
            let snapshot = ba::Snapshot {
                trail_strokes: vec![ba::TrailStroke {
                    points: vec![
                        ba::TrailPoint {
                            position: ba::Point::new(400.0, 500.0),
                            normalized_age: 0.0,
                        },
                        ba::TrailPoint {
                            position: ba::Point::new(600.0, 500.0),
                            normalized_age: 1.0,
                        },
                    ],
                    width,
                }],
                trail_opacity: 1.0,
                global_opacity: 1.0,
                ref_scale: scale,
                ..Default::default()
            };

            let commands = build_commands(&snapshot);
            let trail: Vec<&DrawCommand> = commands
                .iter()
                .filter(|c| c.material == Material::Trail)
                .collect();
            assert_eq!(trail.len(), 1, "一条笔画应铺一段");
            let command = trail[0];
            let room = command.quad.size.h as f64 * 0.5 - command.extent[1] as f64 * 0.5;
            let need = BLOOM_TRAIL_HALO_PX * scale as f64;
            assert!(
                room + 1e-6 >= need,
                "输出高 {height}：留白 {room}px 容不下 {need}px 的光晕"
            );
            // 传进着色器的倍率必须和留白用的是同一个。
            assert!((command.ref_scale - scale).abs() < 1e-6);
        }
    }

    #[test]
    fn trail_segments_are_contiguous_and_progress_monotonic() {
        let snapshot = click_snapshot();
        let commands: Vec<DrawCommand> = build_commands(&snapshot)
            .into_iter()
            .filter(|c| c.material == Material::Trail)
            .collect();
        assert!(!commands.is_empty(), "按住拖动应产生拖尾段");
        let mut previous = f32::INFINITY;
        for command in &commands {
            // 纹理 u 从 1 递减到 0，所以 progress.x 单调递减。
            assert!(command.progress[0] <= previous + 1e-5);
            previous = command.progress[0];
            // 条带宽度即拖动宽度。
            assert!((command.extent[1] - command.size).abs() < 1e-6);
            assert!(command.extent[0] > 0.0);
        }
    }
}
