//! 光标特效 GLES 渲染层（Cursor Effects Rendering）。
//!
//! 复用仓库既有的 `ShaderRenderElement`（与 `BorderRenderElement` 同模板），把
//! `CursorEffectState` 中的粒子（爆裂圆盘/外圈环/火花/拖尾/滚轮圆环）翻译为
//! 字形粒子（glyph atlas 采样；`glyphs.rs`）。
//!
//! 坐标空间：quad 用 `niri_v_coords`（归一化 `[k, k+1]`），shader 通过 `input_to_geo`
//! 矩阵（= `Mat3::from_scale(area_size)`，等价 `border.rs` 的 `geo_loc=0` 情形）转到
//! 几何像素空间（Canvas 风格左上原点）。所有 `u_center` 都用这个空间。
//! 没有 `input_to_geo`，字形 placement 全部塌缩，整个 quad 亮成方块。

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{
    ffi, GlesError, GlesFrame, GlesRenderer, GlesTexture, Uniform,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::cursor_effect::glyphs;
use crate::cursor_effect::state::{self, hash_unit, CursorEffectState};
use crate::render_helpers::renderer::{AsGlesFrame as _, NiriRenderer};
use crate::render_helpers::shader_element::ShaderRenderElement;
use crate::render_helpers::shaders::{mat3_uniform, ProgramType, Shaders};

/// 一个光标特效粒子基元的渲染参数。
#[derive(Debug, Clone, Copy)]
struct Params {
    color: [f32; 3],
    center: [f32; 2],
    radius: f32,
    alpha: f32,
    glyph: f32,
    rot: f32,
}

/// 光标特效里一个粒子基元的渲染元素（与 `BorderRenderElement` 同模板）。
#[derive(Debug, Clone)]
pub struct CursorEffectElement {
    inner: ShaderRenderElement,
}

impl CursorEffectElement {
    /// 组装一个字形粒子 quad 并绑定全部 uniform / 纹理。
    fn build(
        size: Size<f64, Logical>,
        loc: Point<f64, Logical>,
        params: Params,
        scale: f32,
        atlas: &GlesTexture,
    ) -> Self {
        let area = Vec2::new(size.w as f32, size.h as f32);
        // 几何充满整个 quad，`geo_loc=0`，所以 `input_to_geo = Scale(area)`，
        // `coords_geo = niri_v_coords * area`。
        let input_to_geo = Mat3::from_scale(area);

        let uniforms: Vec<Uniform> = vec![
            mat3_uniform("input_to_geo", input_to_geo),
            Uniform::new("u_color", params.color),
            Uniform::new("u_center", params.center),
            Uniform::new("u_radius", params.radius),
            // 字形索引 + 旋转（Rust 侧按 ~200ms 节拍闪烁，相位打散）。
            Uniform::new("u_glyph", params.glyph),
            Uniform::new("u_rot", params.rot),
        ];

        let textures = HashMap::from([(String::from("cursor_glyph_atlas"), atlas.clone())]);

        let mut inner = ShaderRenderElement::empty(ProgramType::CursorEffect, Kind::Unspecified);
        inner.update(
            size,
            None,
            scale,
            params.alpha,
            Rc::from(uniforms),
            textures,
        );
        inner = inner.with_location(loc);
        Self { inner }
    }

    /// 把全局逻辑坐标减去 output 左上 → 输出局部坐标。
    fn local(center_global: Point<f64, Logical>, output_loc: Point<f64, Logical>) -> (f64, f64) {
        (
            center_global.x - output_loc.x,
            center_global.y - output_loc.y,
        )
    }

    /// 字符模式（mode 4）：随机代码字符，atlas 采样字形掩码。
    /// `center_global` 字形中心、`half_size` 字形半边长（geo px）、`glyph_index`
    /// 字形索引（0..=`glyphs::CHAR_COUNT-1`）、`rot` 旋转弧度。
    pub fn glyph(
        center_global: Point<f64, Logical>,
        half_size: f32,
        glyph_index: f32,
        rot: f32,
        color: [f32; 3],
        alpha: f32,
        output_loc: Point<f64, Logical>,
        scale: f32,
        aa: f32,
        atlas: &GlesTexture,
    ) -> Self {
        let (lx, ly) = Self::local(center_global, output_loc);
        let r = half_size.max(0.5) as f64;
        // quad 需包住字形 + 抗锯齿软边；shader 内 u_radius=字形半边长。
        let rc = r + aa as f64;
        let size = Size::from((rc * 2., rc * 2.));
        let loc = Point::from((lx - rc, ly - rc));
        Self::build(
            size,
            loc,
            Params {
                color,
                center: [rc as f32, rc as f32],
                radius: r as f32,
                alpha,
                glyph: glyph_index,
                rot,
            },
            scale,
            atlas,
        )
    }

    /// GLES shader 是否就绪。
    pub fn has_shader(renderer: &mut impl NiriRenderer) -> bool {
        Shaders::get(renderer)
            .program(ProgramType::CursorEffect)
            .is_some()
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
            // 加法混合做发光叠加：frag 输出为预乘 alpha (`gl_FragColor = vec4(rgb*alpha, alpha)`)，
            // 故用 `BlendFunc(ONE, ONE)`（预加色与预加 alpha 项），画完还原
            // smithay 默认 `BlendFunc(ONE, ONE_MINUS_SRC_ALPHA)`（预乘 over）。
            frame.with_context(|gl| unsafe {
                gl.BlendFunc(ffi::ONE, ffi::ONE);
            })?;
            let res = RenderElement::<GlesRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            );
            frame.with_context(|gl| unsafe {
                gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
            })?;
            res
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

/// `weight-prop(t) = min(2 - |4(t - 0.5)|, 1)`；对应描边宽度沿段中央的峰。
#[inline]
fn weight_prop(t: f32) -> f32 {
    (2.0 - (4.0 * (t - 0.5)).abs()).min(1.0)
}

/// 收集一个输出上需要绘制的光标特效元素（爆裂圆盘/外圈环/火花/拖尾/滚轮圆环）。
///
/// 所有坐标都先转成输出局部坐标（screen-global 减 output_loc）。
///
/// 字符模式：当 `cursor_effect_atlas` 纹理可用时（`Shaders::compile` 时创建），
/// 全部粒子（点击爆裂/外圈/火花/拖尾）渲染为随机代码字符（a-z/A-Z/数字/符号，
/// 稳定伪随机数 [0,1)：同一 seed 恒返回同一值（字符生命周期内恒定），
/// 用于给每个字符派生独立的速度/半径/大小/淡出时间 → 不规则运动与消失。

/// 字符模式：全部粒子替换为代码字符（`glyphs.rs`），每 ~200ms 换一次字形
/// （`glyph_for` 相位打散，各字符在不同时刻闪烁）。
pub fn collect_render_elements(
    renderer: &mut impl NiriRenderer,
    state: &CursorEffectState,
    output_loc: Point<f64, Logical>,
    scale: f32,
    aa: f32,
) -> Vec<CursorEffectElement> {
    if !state.enabled {
        return Vec::new();
    }
    let Some(atlas) = Shaders::get(renderer).cursor_effect_atlas.clone() else {
        return Vec::new();
    };
    collect_glyph_elements(state, output_loc, scale, aa, &atlas)
}

/// 字符模式：全部粒子替换为随机代码字符（`glyphs::FONT`，ASCII 0x21..0x7E）。
fn collect_glyph_elements(
    state: &CursorEffectState,
    output_loc: Point<f64, Logical>,
    scale: f32,
    aa: f32,
    atlas: &GlesTexture,
) -> Vec<CursorEffectElement> {
    let mut out = Vec::with_capacity(64);

    // 字符闪烁节拍：每 ~200ms 换一次字符；相位按 seed 打散（STAGGER 档），
    // 不同字符在不同时刻变化——不会全体同时闪。
    let t_ms = state
        .last_frame_time
        .duration_since(state.instant0)
        .as_secs_f64()
        * 1000.0;
    const CHANGE_MS: f64 = 200.0;
    const STAGGER: usize = 12;
    let glyph_for = |seed: usize| -> f32 {
        let phase_ms = (seed % STAGGER) as f64 * (CHANGE_MS / STAGGER as f64);
        let bucket = ((t_ms + phase_ms) / CHANGE_MS).floor() as u64;
        let h = bucket.wrapping_mul(0x85EBCA77) ^ (seed as u64).wrapping_mul(0xC2B2AE35);
        (h as usize % glyphs::CHAR_COUNT) as f32
    };

    let opacity = state.opacity;

    // ─── Waves: filledCircle + rings → 字符 ───
    for w in state.waves.iter() {
        // 运动：半径/环位置仍由 MAX_LIFE 推进（速度不变）。
        let wave_prog = (w.life / state::filled_cfg::MAX_LIFE).clamp(0.0, 1.0);
        let ring_prog = (w.life / state::rings_cfg::MAX_LIFE).clamp(0.0, 1.0);
        // 淡出：用 FADE_EXTEND 延长窗口 → 消失更慢。
        let wave_fade = (w.life / (state::filled_cfg::MAX_LIFE * state::filled_cfg::FADE_EXTEND))
            .clamp(0.0, 1.0);
        let ring_fade =
            (w.life / (state::rings_cfg::MAX_LIFE * state::rings_cfg::FADE_EXTEND)).clamp(0.0, 1.0);

        let ease = 1.0 - (1.0 - wave_prog).powi(3);
        let fill_r = state::filled_cfg::R_ADD_RATE * state.scale * ease;
        let fill_alpha = (1.0 - wave_fade).max(0.0) * opacity;
        if fill_alpha > 0.0 {
            // 爆裂圆盘（点阵铺满圆）。
            let r = fill_r.max(5.0);
            // 圆盘字符只铺到 0.85r：圆环（在 r 处）落在盘外，不压盘边字符。
            let disk_r = r * 0.85;
            let spacing = (disk_r / 3.5).max(4.0);
            let cell = (disk_r / spacing).ceil() as i32;
            let mut cell_i = 0usize;
            for gy in -cell..=cell {
                for gx in -cell..=cell {
                    let dx = gx as f32 * spacing;
                    let dy = gy as f32 * spacing;
                    if dx * dx + dy * dy > disk_r * disk_r {
                        continue;
                    }
                    cell_i += 1;
                    let seed_c = (w.seed as usize).wrapping_mul(0x9E37_79B9) ^ (70000 + cell_i);
                    // 每格独立不规则：位置抖动、大小、亮度、淡出时间全部派生自 seed。
                    let jx = (hash_unit(seed_c) - 0.5) * spacing * 0.6;
                    let jy = (hash_unit(seed_c ^ 0x3C1) - 0.5) * spacing * 0.6;
                    // 更大更实：尺寸 1.2..1.5×间距（重叠多）、亮度 0.85..1.0。
                    let sz = (spacing * (1.2 + hash_unit(seed_c ^ 0x5A5) * 0.3)).max(5.0);
                    let brightness = 0.85 + hash_unit(seed_c ^ 0x1E1) * 0.15; // 0.85..1.0
                                                                              // 淡出时间不规则且缓慢：起点 0..0.4、终点 0.5..1.0 散开
                                                                              // （最长差 ~32 帧）→ 各格明显在不同时刻消失。
                    let fade_start_c = hash_unit(seed_c ^ 0x9E3) * 0.4; // 0..0.4
                    let fade_end_c = 0.5 + hash_unit(seed_c ^ 0xF1) * 0.5; // 0.5..1.0
                    let cell_fade =
                        ((wave_fade - fade_start_c) / (fade_end_c - fade_start_c)).clamp(0.0, 1.0);
                    let alpha_c = (1.0 - cell_fade) * fill_alpha * brightness;
                    if alpha_c <= 0.0 {
                        continue;
                    }
                    out.push(CursorEffectElement::glyph(
                        Point::from((
                            w.x as f64 + dx as f64 + jx as f64,
                            w.y as f64 + dy as f64 + jy as f64,
                        )),
                        sz,
                        glyph_for(seed_c),
                        0.0,
                        w.color,
                        alpha_c,
                        output_loc,
                        scale,
                        aa,
                        atlas,
                    ));
                }
            }
        }

        let ring_alpha = state.ring_alpha(ring_fade);
        let ring_rgb = state::CursorEffectState::ring_rgb_at_with(w.rings_end_color, ring_prog);
        if ring_alpha <= 0.0 {
            continue;
        }
        for (seg_idx, seg) in w.ring.segs.iter().enumerate() {
            let seg_off = seg.off;
            // 环淡出不"向固定点收缩"，字符始终分布在整段弧上；淡出改为
            // 每个字符独立不规则的透明度渐变 → 缓慢透明消失、不一起没。
            // （保留 LEN_STOP_ADD 阶段的"环从弧头生长出来"）
            let len = if ring_fade <= state::rings_cfg::LEN_STOP_ADD_POINT {
                let frac = if state::rings_cfg::LEN_STOP_ADD_POINT > 0.0 {
                    ring_fade / state::rings_cfg::LEN_STOP_ADD_POINT
                } else {
                    1.0
                };
                seg.len * frac
            } else {
                seg.len
            };

            let radius = w.r + seg.r_round_rate * state.scale;
            let seg_num = state::rings_cfg::SEG_NUM;
            for k in 0..seg_num {
                let t0 = k as f32 / seg_num as f32;
                let t1 = (k + 1) as f32 / seg_num as f32;
                // 种子只依赖 wave 自身 seed（不依赖 waves 数组下标 wi —— wi 会随
                // 前序 wave 回收后移而改变 → 字符速度/字形/淡出中途突变 = 闪现）。
                // 用 seg 下标区分两段，避免两段字符完全镜像。
                let seed_k = (w.seed as usize).wrapping_mul(0x9E37_79B9)
                    ^ (seg_idx.wrapping_mul(0x100) + k) as usize;
                // 每字符独立角速度系数 0.6..1.4（继承环旋转）。
                let speed_k = 0.6 + hash_unit(seed_k ^ 0x11) * 0.8;
                // 每字符独立自转速度 0.04..0.12 rad/帧：有下限、各不相同（像落叶），
                // 保证即使环本身不转（rs=0）每个字符也在动。
                let spin_k = 0.04 + hash_unit(seed_k ^ 0x55) * 0.08;
                // 每字符独立半径 0.85..1.15 → 环不再是一个正圆，逐个漂移。
                let rad_k = 0.85 + hash_unit(seed_k ^ 0x22) * 0.3;
                // 每字符独立淡出窗口：起点 0.05..0.45、时长 0.12..0.62 → 各字符
                // 在完全不同时刻淡完（明显逐个消失），而非整环同一时刻一起没。
                let fade_start_k = 0.05 + hash_unit(seed_k ^ 0x33) * 0.4; // 0.05..0.45
                let fade_end_k = (fade_start_k + 0.12 + hash_unit(seed_k ^ 0x44) * 0.5).min(1.0); // 终点 0.17..1.0，时长 0.12..0.62
                let fade_prog =
                    ((ring_fade - fade_start_k) / (fade_end_k - fade_start_k)).clamp(0.0, 1.0);
                let a0 = seg_off + len * t0;
                let a1 = seg_off + len * t1;
                if (a1 - a0).abs() < 0.01 {
                    continue;
                }
                let mid_arc = (a0 + a1) * 0.5;
                // 位置 = 弧内相对角 + 继承环旋转 + 每字符独立自转（随时间累加）。
                let g_angle = mid_arc + w.ring.ang * speed_k + w.life * spin_k;
                let g_radius = radius * rad_k;

                let w_t = weight_prop(t0);
                let lw = (state::rings_cfg::MIN_W * (1.0 - w_t) + state::rings_cfg::MAX_W * w_t)
                    * (-0.8 * (ring_fade - 0.8) + 1.0).min(1.0);
                if lw <= 0.0 {
                    continue;
                }
                // 每字符独立淡出窗口 → 消失时间不规则，缓慢渐隐。
                let char_alpha = (1.0 - fade_prog) * ring_alpha;
                if char_alpha <= 0.0 {
                    continue;
                }
                let gx = w.x + g_radius * g_angle.cos();
                let gy = w.y + g_radius * g_angle.sin();
                out.push(CursorEffectElement::glyph(
                    Point::from((gx as f64, gy as f64)),
                    (lw * 3.0).max(5.5),
                    glyph_for(seed_k),
                    0.0,
                    ring_rgb,
                    char_alpha,
                    output_loc,
                    scale,
                    aa,
                    atlas,
                ));
            }
        }
    }

    // ─── Sparks → 字符（顶点旋转） ───
    for (si, s) in state.sparks.iter().enumerate() {
        let alpha = s.a.max(0.0) * opacity;
        if alpha <= 0.0 {
            continue;
        }
        out.push(CursorEffectElement::glyph(
            Point::from((s.x as f64, s.y as f64)),
            (s.s * 1.3).max(6.0),
            glyph_for(5000 + si),
            s.rot,
            s.color,
            alpha,
            output_loc,
            scale,
            aa,
            atlas,
        ));
    }

    // ─── Trail → 字符链（拖尾路径点，逐点字符） ───
    // 拖尾点间距 ~2px；字符放大到 13px、每 ~10px 一个 → 少而大、代码流风格。
    let head = if state.is_down || state.persistent_trail {
        state.last_pos
    } else {
        None
    };
    let n = state.trail.len();
    for (i, p) in state.trail.iter().enumerate().step_by(5) {
        if p.life <= 0.0 {
            continue;
        }
        let t = if n > 1 {
            i as f32 / (n as f32 - 1.0)
        } else {
            1.0
        };
        // 尾→头线性渐变：尾部淡、头部亮（尾底抬高到 0.12，长拖尾整条可见）。
        let alpha = (0.12 + t * 0.83).min(0.95);
        out.push(CursorEffectElement::glyph(
            Point::from((p.x as f64, p.y as f64)),
            9.0,
            glyph_for(p.id as usize),
            0.0,
            p.color,
            alpha,
            output_loc,
            scale,
            aa,
            atlas,
        ));
    }
    if let Some((hx, hy)) = head {
        out.push(CursorEffectElement::glyph(
            Point::from((hx as f64, hy as f64)),
            11.0,
            glyph_for(12345),
            0.0,
            state.color_norm(),
            1.0,
            output_loc,
            scale,
            aa,
            atlas,
        ));
    }

    // ─── ScrollRings: 滚轮代码圆环（下滚淡橙顺时针 / 上滚淡绿逆时针） ───
    // 滚动中整环转动；停止滚动后按 idle 逐个字符不规则淡出（类似点击爆裂环）。
    for (ri, r) in state.scroll_rings.iter().enumerate() {
        let rad = state::scroll_cfg::RADIUS * (state.scale / 1.5);
        let p = ((r.idle as f32 - state::scroll_cfg::GRACE_FRAMES as f32)
            / state::scroll_cfg::FADE_FRAMES)
            .clamp(0.0, 1.0);
        for k in 0..state::scroll_cfg::CHARS {
            let seed_c = (r.seed as usize) ^ (90000 + ri * 4096 + k);
            // 每字符独立淡出窗口（起点 0..0.35、终点 0.45..1.0）→ 逐个消失。
            let fade_start_c = hash_unit(seed_c ^ 0xA11) * 0.35;
            let fade_end_c = 0.45 + hash_unit(seed_c ^ 0xB22) * 0.55;
            let fa = ((p - fade_start_c) / (fade_end_c - fade_start_c)).clamp(0.0, 1.0);
            let alpha = (1.0 - fa) * opacity;
            if alpha <= 0.0 {
                continue;
            }
            let base = k as f32 / state::scroll_cfg::CHARS as f32 * TAU;
            let a = base + r.ang;
            let px = r.x + a.cos() * rad;
            let py = r.y + a.sin() * rad;
            let sz = 10.0 + hash_unit(seed_c ^ 0xC33) * 4.0;
            out.push(CursorEffectElement::glyph(
                Point::from((px as f64, py as f64)),
                sz,
                glyph_for(seed_c),
                0.0,
                r.color,
                alpha,
                output_loc,
                scale,
                aa,
                atlas,
            ));
        }
    }

    out
}
