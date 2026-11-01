//! 光标特效 GLES 渲染层（Cursor Effects Rendering）。
//!
//! 分两层：
//!
//! 1. [`build_commands`]：把一帧 `Snapshot` 翻成纯数据（[`DrawCommand`]）——四边
//!    形的屏幕矩形、材质种类、该材质需要的全部参数。不含任何 GL 对象，因此可以
//!    离线跑（单元测试、以及和上游 C++ 参考实现比对）。
//! 2. [`collect_render_elements`]：把 `DrawCommand` 包成 `ShaderRenderElement` （与
//!    `BorderRenderElement` 同模板），绑定 uniform 与纹理。
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
use std::sync::OnceLock;
use std::time::Instant;

use glam::{Mat3, Vec2};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{
    ffi, GlesError, GlesFrame, GlesRenderer, GlesTexture, Uniform,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{
    Bind as _, Color32F, ContextId, Frame as _, ImportMem as _, Offscreen as _, Renderer as _,
    Texture as _,
};
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

/// 语义化调试开关 `CE_SEG=n`：只把第 n 段拖尾送进离屏缓冲。
///
/// 离线诊断用 —— 把每段单独渲染出来逐像素取最大值，应当等于整条笔画的渲染
/// 结果，这是「`GL_MAX` 等价于并集」的直接验证。
///
/// 用 `OnceLock` 缓存：`std::env::var` 每次都要锁环境表并分配一个 `String`，
/// 而这条查询在拖尾的**每帧**路径上 —— 合成器 144Hz 下白烧几十万次分配。
fn ce_only_segment() -> Option<usize> {
    static CACHE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CE_SEG")
            .ok()
            .and_then(|value| value.parse().ok())
    })
}

/// 语义化调试开关 `CE_DUMP=1`：打印拖尾命令与离屏缓冲的混合方程。同上缓存。
fn ce_dump_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CE_DUMP").is_some())
}

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

/// 与 `cursor_effect.frag` 的 `BLOOM_TRAIL_SIGMA_MAX`（48）配套：截断窗已删，
/// σ=48 高斯尾巴要到 ~165px 才降到 <0.3/255，四边形必须盖过它。
///
/// 光晕是屏幕空间的：半径只跟输出尺寸有关，与实际带宽无关。所以留白是「180 个
/// 1080p 像素 × 输出倍率」，不再按带宽的倍数给。
const BLOOM_TRAIL_HALO_PX: f64 = 180.0;
/// 拖尾四边形沿切线前后各外扩的长度（1080p 参考像素 × 输出倍率）。
///
/// 与 `cursor_effect.frag` 的 `BLOOM_TRAIL_JOINT_PX` 必须一致：shader 拿它把
/// 四边形里的 `along` 反算回真实段内坐标（`u_extent.x - 2 × 外扩`），两边差一个
/// 像素，亮带就会沿切线错位到相邻段的地盘上。
const BLOOM_TRAIL_JOINT_PX: f64 = 22.0;

/// 圆盘外晕的屏幕空间半径（1080p 参考像素），与 `cursor_effect.frag` 的
/// `DISK_HALO_SIGMA_MAX`（96）配套。
///
/// 圆盘是点击特效的视觉锚点：官方图里那圈蔚蓝的雾一直铺到盘边外 ~130px，靠
/// `BLOOM_QUAD_SCALE` 那个「按图元倍数」的留白远远撑不到 —— 圆盘本身只有几十
/// 像素，乘 2.4 还是几十像素，雾会被四边形切成一个方块。所以这里和拖尾一样按
/// 屏幕像素给。
///
/// 110 → 190：`DISK_HALO_END` 截断窗删除后，σ=96 高斯尾巴要铺到 ~172px 才降
/// 到 <1/255。留白必须盖过核的自然收尾位置，否则雾在四边形边界上被直切 ——
/// 实机截图里雾被弧形暗边啃掉一块，就是 110px 的四边形边干的。
const DISK_HALO_PX: f64 = 190.0;

/// 环外晕的屏幕空间半径（1080p 参考像素），与 `cursor_effect.frag` 的
/// `RING_GLOW_SIGMA_MAX`（96）配套。
///
/// 环的外晕在参考站截图里铺到环外缘外约 55px（主瓣 τ=5px + 残尾 τ=40px）。
/// `BLOOM_QUAD_SCALE` 的 2.4 倍只比环本身多出几十像素，在边界上雾还有余量 ——
/// 会被切出一道直角台阶，所以和圆盘一样按像素外扩。
///
/// 108 → 190：`RING_GLOW_END` 截断窗删除后，σ=96 高斯尾巴要铺到 ~172px 才降到
/// <1/255。留白不足时雾在四边形边界上被直切，斜向边角还会切出尖角。
const RING_HALO_PX: f64 = 190.0;

/// 三角碎片外晕的屏幕空间半径（1080p 参考像素），与 `cursor_effect.frag` 的
/// `TRI_GLOW_SIGMA_MAX`（24）配套。
///
/// 旧实现这里是 `0`：碎片的光晕被塞进「刚好裹住三角形」的那个四边形里，只能
/// 在形状内部糊成一团圆光，一个像素都铺不出去。既然光晕改成跟着三角形 SDF
/// 走，四边形就必须比三角形本身大一圈。
///
/// 44 → 64：`TRI_GLOW_END` 截断窗删除后，σ=24 高斯尾巴要铺到 ~54px 才降到
/// <1/255，留白盖到 64px。上限仍是「不要糊成一团白烟」：四个碎片的光在彼此
/// 靠近时会连成一片，而官方参考图里碎片外缘基本紧贴、几乎看不出光晕 —— 所以
/// 只放到核自己收干的位置，不再多给。
const TRIANGLE_HALO_PX: f64 = 64.0;

/// 辉光增益，等于上游 Bloom 的最终乘数。
///
/// 上游 `config.js` 里写的是 `bloom.intensity = 1.7`，但 Shader 收到的不是这个
/// 序列化值 —— 游戏在绑定前先过 `resolveUnityBloomIntensity()`：
///
/// ```js
/// expm1(intensity / 10 * Math.LN2)   // = 2^(1.7/10) - 1 = 0.125
/// ```
///
/// 所以 Unity 那边真正的乘数是 **0.125**，不是 1.7，也不是我们之前用的 0.25。
/// 早先按官方演示图的 sRGB 剖面反解出 0.25（「圆盘内部约 +25%」），是把 sRGB
/// 压缩当成了线性亮度 —— 和上一轮环外晕的 τ 犯的是同一个错。改成源码口径后，
/// 近场雾正好落到参考站的水平。
///
/// 拖尾原本靠这一个值，现在它被减半，用 `glow { trail-gain }` 单独补回（配置里
/// 调到 1.1 即与旧的 0.25 × 0.55 等价），两边不互相牵连。
///
/// 另一处补偿在 shader 里：上游的 bloom 是**能量守恒**的多级 mip 模糊，一条环带
/// 的能量被摊到 60px 以上；我们是 SDF + 指数衰减的解析近似，总能量天生偏小。
/// 所以每层的形状增益都按 2 倍给（`RING_GLOW_NEAR_GAIN` 等），乘上这里的 0.125
/// 之后正好回到与旧版 `0.25 × 原增益` 相同的绝对亮度 —— 亮度不变，只是不再用
/// over 覆盖糊住本体。
const BLOOM_GAIN: f32 = 0.125;

/// 材质种类，值与 `cursor_effect.frag` 的 `u_mode` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// 拖尾：段末端（相对四边形左上角的像素）。
    ///
    /// 直接给**原始端点**，不让 shader 反算：相邻两段对同一个点必须算出逐位
    /// 相同的距离，`owns` 的两条判据才严格互补。
    pub seg_end: [f32; 2],
    /// 拖尾：段长与带宽（像素）。
    pub extent: [f32; 2],
    /// 拖尾：首段 / 尾段标记（1 = 辉光在它这一端收成圆头）。
    pub cap: [f32; 2],
    /// 输出高度相对 1080p 的倍率（辉光半径的像素口径）。
    pub ref_scale: f32,
    /// 辉光增益倍率（乘到 `BLOOM_GAIN` 上）。
    pub glow_gain: f32,
    /// 额外作用在**拖尾**雾上的增益倍率（非拖尾材质恒为 1.0）。
    /// 让「拖尾更淡」和「点击辉光保持原样」可以分别调。
    pub trail_glow_gain: f32,
    /// 辉光半径倍率（乘到所有雾的距离上）。
    pub glow_radius: f32,
    /// 溶解环内径比（外径的倍数）。
    pub ring_inner: f32,
    /// 这一遍画的是辉光（纯加法）还是本体（预乘 over）。
    ///
    /// 上游把两者放在两张 RT 里，最后以 `One/One` 把辉光加回 Scene。我们做不到
    /// 每图元换 blend func（smithay 每画一个图元都会重设 `glBlendFunc`），所以
    /// 用等价的写法：辉光遍在 shader 里输出 `alpha = 0`，预乘 over 就退化成
    /// `dst += rgb`。
    ///
    /// 同一个图元会展开成两条命令，`build_commands` 末尾按本字段稳定排序，保证
    /// **所有本体画完之后**才是辉光 —— 否则后画的 over 会把先画的加法结果盖掉。
    pub glow_pass: bool,
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
        let radius = if base.glow_radius.is_finite() && base.glow_radius > 0.0 {
            base.glow_radius as f64
        } else {
            1.0
        };
        match material {
            Material::Disk => DISK_HALO_PX * scale * radius,
            // 环的雾现在也按屏幕像素给（见 shader 的 `RING_GLOW_NEAR_LEN`），
            // 所以四边形同样要按像素外扩：`BLOOM_QUAD_SCALE` 那 2.4 倍只够环
            // 自己那几十像素，雾会在四边形边界上被切出一圈直角台阶。
            Material::Ring => RING_HALO_PX * scale * radius,
            // 碎片的雾改成沿三角形 SDF 衰减，同样必须铺到形状外 —— 留白为 0
            // 时那圈光只能憋在三角形自己的包围盒里，看起来就是「中心一团圆光」。
            Material::Triangle => TRIANGLE_HALO_PX * scale * radius,
            Material::Trail => 0.0,
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
            seg_end: [side as f32, side as f32],
            extent: [side as f32, side as f32],
            cap: [0.0, 0.0],
            ref_scale: base.ref_scale,
            glow_gain: base.glow_gain,
            trail_glow_gain: base.trail_glow_gain,
            glow_radius: base.glow_radius,
            ring_inner: base.ring_inner,
            glow_pass: false,
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
    glow_gain: f32,
    trail_glow_gain: f32,
    glow_radius: f32,
    ring_inner: f32,
}

/// 绘制序号（小 = 先画 = 在更底下）。
///
/// 顺序**照抄上游的提交顺序**，不是按「谁亮谁在上」猜的。参考实现的
/// `_renderWebGLScene` 一段一段 append：
///
/// ```text
/// trail strokes                        // 先画拖尾笔画
/// wave.appendWebGLSceneDiskLayer()      // Cross2 盘，ONE/ONE_MINUS_SRC_ALPHA
/// wave.appendWebGLSceneAdditiveLayer()  // 同一队列 4499：hit / flare / **dissolve ring**
/// shard.appendWebGLScene()              // Tri2，RenderQueue 4550，最后提交
/// ```
///
/// 也就是说 **环是画在盘之上的**：环的半径从「盘的 42%」一路长到「盘的
/// 1.06~1.24 倍」，早期整条白环就压在蓝盘面上，随着生长扫到盘外 —— 参考图里
/// 「点击瞬间盘面上就有一圈白环」正是这个。曾经为了「盘边被环的内沿渐变推白」
/// 把环整层压到盘下面，结果环在 0~200ms（盘还实心的时候）被盘本体完全盖住，
/// 实机就变成「点击只有蓝盘、没有光环」。
///
/// **雾（glow_pass）默认垫在最底，本体压在雾上面** —— 唯一例外是环的雾。
///
/// 这是「漏光」的唯一正确解法，靠调增益是修不掉的。雾是 alpha=0 的纯加法，
/// 本体是预乘 over：本体画在雾之上时，盘面按自己的 alpha 把雾**扣掉**，盘内
/// 干净、盘外那圈光晕原样留着；反过来让雾后画（哪怕增益压到 0.002）它也会
/// 无差别地加在盘面上，整块盘读成一层糊住表面、没有层次的半透明亮膜。
///
/// 上游的 bloom 是独立一遍、对整帧卷积后叠回 Scene，看起来像是「雾在最上」；
/// 但 Unity 的 Overlay 相机叠的是**已经合成好的整帧**，盘本体在那一遍里的
/// 贡献是「不透明像素」，bloom 只从盘的**外沿**取样。等效到我们这种逐图元
/// 的加法遍，就是「雾在底、本体盖住雾」，而不是「雾糊在本体上」。
///
/// **环是例外**：它的内沿在参考里是**糊在盘面上**的一圈白光（离环带 10px 处
/// 盘面已经比盘心亮 30 级），排到盘下面会被不透明的盘本体整片扣掉，只剩一条
/// 硬边细线 + 一圈贴在盘外的雾 —— 实机就是「环里还套着一条更淡的环」。
/// 所以环的雾夹在盘本体之后、环本体之前。盘自己的雾不享受这个待遇：盘内那圈
/// 亮度由本体自己的发射项（`DISK_RIM_*`）给，那是「自发光」，与雾无关。
///
/// 顺序：盘雾 → 碎片雾 → 拖尾雾 → 拖尾 → 盘本体 → **环雾** → 环本体 → 碎片本体。
fn draw_rank(material: Material, glow_pass: bool) -> u8 {
    match (material, glow_pass) {
        // 环的雾夹在盘本体与环本体之间（理由见上）。
        (Material::Ring, true) => 3,
        // 其余三种雾合并成同一档铺在最底。
        (_, true) => 0,
        // 上游把拖尾当连续笔画，先于所有点击粒子提交；但它自己发光，要压在
        // 雾上面（否则拖尾会被盘的雾整体加亮、边缘发糊）。
        (Material::Trail, false) => 1,
        (Material::Disk, false) => 2,
        (Material::Ring, false) => 4,
        (Material::Triangle, false) => 5,
    }
}

/// 把配置里的整体着色乘到美术原色上。`[1,1,1]` 时原样返回。
fn tinted(color: [f32; 3], tint: [f32; 3]) -> [f32; 3] {
    [color[0] * tint[0], color[1] * tint[1], color[2] * tint[2]]
}

/// 把一帧快照翻成绘制命令列表（纯函数，按绘制顺序排列）。
/// 把配置里的数值夹到「画得出来」的范围。
///
/// 用户可以在 `config.kdl` 里随便写，所以这里兜住三种会让画面直接坏掉的值：
/// - `glow-radius` 在着色器里是除数，0 会算出 inf/NaN（整块四边形变成白块）；
/// - `inner-ratio ≥ 1` 时内径不小于外径，环退化成完全透明；
/// - 增益用一个宽松上限挡住手滑写出的天文数字。
fn sanitize(visual: &ba::VisualConfig) -> ba::VisualConfig {
    let mut v = *visual;
    v.glow_radius = if v.glow_radius.is_finite() {
        v.glow_radius.clamp(0.05, 64.0)
    } else {
        1.0
    };
    v.glow_gain = if v.glow_gain.is_finite() {
        v.glow_gain.clamp(0.0, 64.0)
    } else {
        1.0
    };
    v.glow_trail_gain = if v.glow_trail_gain.is_finite() {
        v.glow_trail_gain.clamp(0.0, 64.0)
    } else {
        1.0
    };
    v.ring_inner_ratio = if v.ring_inner_ratio.is_finite() {
        v.ring_inner_ratio.clamp(0.0, 0.999)
    } else {
        0.940_127_5
    };
    v
}

pub fn build_commands(snapshot: &ba::Snapshot, visual: &ba::VisualConfig) -> Vec<DrawCommand> {
    let visual = &sanitize(visual);
    let mut out = Vec::with_capacity(64);

    // 拖尾先画（上游把拖尾当作粒子底下的连续笔画）。
    build_trail(snapshot, visual, &mut out);

    let mut sprites: Vec<&ba::Sprite> = snapshot.sprites.iter().collect();
    sprites.sort_by_key(|sprite| sprite.render_queue);
    for sprite in sprites {
        if !(sprite.size > 0.0) || !sprite.size.is_finite() {
            continue;
        }
        let base = Base {
            color: tinted(sprite.color.to_array(), visual.tint),
            color_a: sprite.color.a,
            gop: snapshot.global_opacity,
            intensity: sprite.intensity,
            threshold: sprite.dissolve_threshold,
            ref_scale: snapshot.ref_scale,
            glow_gain: visual.glow_gain,
            // 精灵（盘/环/碎片）不吃拖尾那一份额外增益。
            trail_glow_gain: 1.0,
            glow_radius: visual.glow_radius,
            ring_inner: visual.ring_inner_ratio,
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
        // 本体 + 辉光两条命令，共用同一个四边形。顺序在函数末尾统一排序。
        out.push(DrawCommand {
            glow_pass: false,
            ..command
        });
        out.push(DrawCommand {
            glow_pass: true,
            ..command
        });
    }

    // 稳定排序：先按图层、再按「本体在前、辉光在后」。
    out.sort_by_key(|c| draw_rank(c.material, c.glow_pass));

    out
}

/// 拖尾：把相邻两个拖尾点之间贴一条四边形，逐顶点插值渐变颜色与纵向覆盖。
///
/// 多个点击实例同时存在时会有多条独立笔画，逐条单独铺段 —— 跨笔画接起来会画出
/// 合并段内中间点到首尾连线的最大弓高（像素）。
///
/// 每段是一个独立四边形。段与段之间的**雾**靠互补斜坡拼（相邻两段各画一半、
/// 和为 1，见 shader 的 `fog_weight`），**本体**靠「最近段拥有」拼，所以接缝
/// 本身不再产生亮度台阶 —— 但段越长，折线的**棱角**越明显。
///
/// 早期版本为了「少段」把弓高抬到 2.5px（换来段数 50 → 15）。那是硬归属时代
/// 的权衡：接缝本身有台阶，必须靠减少接缝数量来掩盖。现在接缝已经无痕，弓高
/// 就该退回「看不出是折线」的量级：0.6px 的弓高在 12.5px 的半宽上完全不可见，
/// 而画 120px 半径的圆时段长只到 ~sqrt(8×120×0.6) ≈ 24px，棱角小于 12°。
const TRAIL_MERGE_BOW: f64 = 0.6;
/// 单条合并段的最大长度（像素）。拖尾整条可能几百像素，不封顶的话一个四边形
/// 会横跨大半个屏幕，白白吃掉填充率；同时段太长会让互补斜坡变宽、雾的分段感
/// 回来。220px 是「直路上少画几个四边形」与「弯道上跟着曲率走」的折中。
const TRAIL_MERGE_MAX_LENGTH: f64 = 220.0;

/// 把折线点序列按「弓高」合并成若干直线段，返回 `[首点下标, 尾下标]`。
///
/// 合并条件：候选段内所有中间点到首尾连线的距离不超过 `TRAIL_MERGE_BOW`，且
/// 累计长度不超上限。早先用固定夹角（1.5°）判断，鼠标一画圈方向就持续变化，
/// 每 15px 断一次，段长远小于光晕需要接力的 44px —— 拖尾的雾于是一段暗一段
/// 亮，画个圈全暴露出来。改弓高之后，直路上能并出上百像素，弯道上按实际偏离
/// 断开，两者都满足「段长 ≥ 2 × 互补斜坡」。
fn merge_collinear_runs(points: &[ba::TrailPoint], distances: &[f32]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;

    for index in 1..points.len() {
        let projected = (distances[index] - distances[start]) as f64;
        let keep = projected <= TRAIL_MERGE_MAX_LENGTH
            && (start + 1..index).all(|middle| {
                bow(
                    points[middle].position,
                    points[start].position,
                    points[index].position,
                ) <= TRAIL_MERGE_BOW
            });
        if !keep {
            runs.push((start, index - 1));
            start = index - 1;
        }
    }
    runs.push((start, points.len() - 1));
    runs
}

/// 点到直线 `a→b` 的距离（像素）。
fn bow(p: ba::Point, a: ba::Point, b: ba::Point) -> f64 {
    let (abx, aby) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let (apx, apy) = ((p.x - a.x) as f64, (p.y - a.y) as f64);
    let length = (abx * abx + aby * aby).sqrt();
    if length < 1e-9 {
        return (apx * apx + apy * apy).sqrt();
    }
    (apx * aby - apy * abx).abs() / length
}

/// 曲线补点的目标弦长（像素）。
///
/// 只影响「用多少个点去近似这条曲线」，不影响曲线本身。8px 已经远小于拖尾
/// 半宽（12.5px）和光晕的切口容差（`BLOOM_TRAIL_JOINT_PX`），再细只是白花
/// 四边形数量。
const CURVE_DRAW_STEP_PX: f64 = 8.0;
/// 单段最多补多少个点。防止一帧移动 800px 时补出上百段、把四边形数量顶爆。
const CURVE_DRAW_MAX_SUBDIVISIONS: usize = 24;

/// `apply-curve-draw`：用 Catmull-Rom 在采样点之间补点，把折线变成曲线。
///
/// 为什么需要它：`trail-refresh-rate` 默认 40Hz，快速移动时相邻采样点隔开
/// 几十上百像素，原样铺段会把一条弧线画成折线，拐点处两段的外侧张角一拉开
/// 就是一道硬折。补点之后条带的切线逐点变化，折角消失。
///
/// `normalized_age` 按段内参数线性插值 —— 它只用来查颜色/覆盖度曲线，而这两
/// 条曲线在 25ms 的采样间隔上几乎是线性的，不必按弧长重算。
fn resample_curve(points: &[ba::TrailPoint]) -> Vec<ba::TrailPoint> {
    if points.len() < 2 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(points.len() * 4);
    let at = |i: isize| -> ba::TrailPoint {
        let n = points.len() as isize;
        points[(i.clamp(0, n - 1)) as usize]
    };

    for index in 0..points.len() - 1 {
        let p0 = at(index as isize - 1);
        let p1 = points[index];
        let p2 = points[index + 1];
        let p3 = at(index as isize + 2);
        let seg = ((p2.position.x - p1.position.x).powi(2)
            + (p2.position.y - p1.position.y).powi(2))
        .sqrt() as f64;
        let steps = ((seg / CURVE_DRAW_STEP_PX).ceil() as usize)
            .clamp(1, CURVE_DRAW_MAX_SUBDIVISIONS);

        // 末段的前一个点由上一段补过，这里只补 (t, 1] 以免重复。
        let first = if index == 0 { 0 } else { 1 };
        for s in first..=steps {
            let t = s as f32 / steps as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            let f = |a: f32, b: f32, c: f32, d: f32| {
                0.5 * (2.0 * b
                    + (-a + c) * t
                    + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                    + (-a + 3.0 * b - 3.0 * c + d) * t3)
            };
            let x = f(p0.position.x, p1.position.x, p2.position.x, p3.position.x);
            let y = f(p0.position.y, p1.position.y, p2.position.y, p3.position.y);
            // 采样点重合时切线是零向量，曲线退化出 NaN；退回端点保证这一帧
            // 还能画出来（整条笔画丢掉比位置略偏更糟）。
            let x = if x.is_finite() { x } else { p1.position.x };
            let y = if y.is_finite() { y } else { p1.position.y };
            out.push(ba::TrailPoint {
                position: ba::Point::new(x, y),
                normalized_age: p1.normalized_age
                    + (p2.normalized_age - p1.normalized_age) * t,
            });
        }
    }
    out
}

/// 一条并不存在的连线。
fn build_trail(snapshot: &ba::Snapshot, visual: &ba::VisualConfig, out: &mut Vec<DrawCommand>) {    for (points, width) in snapshot.strokes() {
        build_trail_stroke(points, width, snapshot, visual, out);
    }
}

fn build_trail_stroke(
    points: &[ba::TrailPoint],
    width: f32,
    snapshot: &ba::Snapshot,
    visual: &ba::VisualConfig,
    out: &mut Vec<DrawCommand>,
) {
    if points.len() < 2 || !(width > 0.0) || !width.is_finite() {
        return;
    }
    // `apply-curve-draw`：先把采样点补成曲线，再走同一套铺段逻辑。
    let smoothed;
    let points = if visual.curve_draw {
        smoothed = resample_curve(points);
        smoothed.as_slice()
    } else {
        points
    };
    if points.len() < 2 {
        return;
    }
    let ref_scale = if snapshot.ref_scale.is_finite() && snapshot.ref_scale > 0.0 {
        snapshot.ref_scale
    } else {
        1.0
    };
    let glow_radius = if visual.glow_radius.is_finite() && visual.glow_radius > 0.0 {
        visual.glow_radius
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

    // 先把相邻的近共线段并成一条直线段。
    //
    // 拖尾的辉光是一条 40~50px 宽的长条薄板（三段指数的主衰减只有 3.5px），
    // 逐段铺意味着每个接缝处都张开一道楔形缺口；同时 shader 的纵向权重要在接缝
    // 两侧留互补斜坡，斜坡一宽、段内的「平台」就被压成几像素 —— 于是雾沿笔画
    // 方向出现一圈圈明暗条纹，实测截图里那条割裂的拖尾就是这么来的。
    //
    // 合并共线相邻段之后：接缝数下降一个量级，纵向平台恢复到几十像素，两个病
    // 同时缓解。弓高由角度阈值和最大合并长共同限制（50px × 1.5° ≈ 1.3px，小于
    // 拖尾半宽），直线近似不会从带宽里漏出去。
    let runs = merge_collinear_runs(points, &distances);
    let last_point = points.len() - 1;

    let half_width = width * 0.5;
    for &(start, end) in runs.iter() {
        let from = points[start].position;
        let to = points[end].position;
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        let length = (dx * dx + dy * dy).sqrt();
        if !length.is_finite() || length < 1e-3 {
            continue;
        }
        let tangent = (dx / length, dy / length);
        let normal = (-tangent.1, tangent.0);

        // 留白只按**本段**的雾支撑域给。
        //
        // 早先还要再加上「前后各两段」的 reach：那时片元里的雾用五段窗口的
        // `d_min`，落在一个四边形边缘、但离隔壁段只有 30px 的像素依然很亮，
        // 只按自己的半径留白就会在四边形边上把雾硬切一刀（实测孤立 1px 暗线里
        // 20/25 精确落在四边形边界上）。
        //
        // 现在每个四边形只画自己那一段，整条笔画的并集交给 `GL_MAX` 离屏混合
        // （见 [`TrailTarget`]）：某像素的雾值 = 各覆盖它的四边形各自算出的
        // `halo(d_i)` 的最大值，而 `max_i halo(d_i) = halo(min_i d_i)` —— 恰好
        // 就是整条折线的距离场。既不需要窗口，也不需要那圈额外的 reach：
        // 每段只要盖住「自己贡献非零」的范围，并集就自动是完整的。
        //
        // 这条直接砍掉了原先最大的一块填充率开销：快速拖动的折返笔画上，
        // `window_reach` 能把四边形撑到 600px 见方。
        let halo_pad = BLOOM_TRAIL_HALO_PX * ref_scale as f64 * glow_radius as f64;
        // **向外取整到像素网格。**
        //
        // 着色器里的 `pixel` 不是 `gl_FragCoord`，而是
        // `input_to_geo * niri_v_coords`：UV 由 smithay 按**它实际绘制的整数
        // 矩形**插值，再乘以这里的精确尺寸。只要四边形带小数，这个比例就不是
        // 1，映射回来会差最多半个像素 —— 每个四边形的小数部分不同，同一屏幕
        // 像素在相邻两个四边形里就被算成相差 0.5~1px 的不同位置。
        // 取整到整数后 dst 与这里的四边形完全一致，UV 映射精确。
        // 向外取整（floor/ceil）保证覆盖只增不减。
        let pad = halo_pad + half_width as f64 + QUAD_PADDING;
        // 沿切线前后各扩展一段「接缝区」。shader 用 `u_extent.x - 2 × JOINT_PX`
        // 反算段长，两侧必须一致。
        let joint_px = BLOOM_TRAIL_JOINT_PX * ref_scale as f64;
        let (jt, jn) = (tangent.0 as f64 * joint_px, tangent.1 as f64 * joint_px);
        let back = (from.x as f64 - jt, from.y as f64 - jn);
        let fwd = (to.x as f64 + jt, to.y as f64 + jn);
        let min_x = (back.0.min(fwd.0) - pad).floor();
        let max_x = (back.0.max(fwd.0) + pad).ceil();
        let min_y = (back.1.min(fwd.1) - pad).floor();
        let max_y = (back.1.max(fwd.1) + pad).ceil();
        let quad = Rectangle::new(
            Point::from((min_x, min_y)),
            Size::from((max_x - min_x, max_y - min_y)),
        );

        let from_progress = distances[start] / total;
        let to_progress = distances[end] / total;
        // 端点的四边形局部坐标（相对 quad 左上角）。
        let local = |p: ba::Point| [p.x - quad.loc.x as f32, p.y - quad.loc.y as f32];
        out.push(DrawCommand {
            quad,
            material: Material::Trail,
            color: tinted(
                ba::trail::evaluate_unity_trail_color(from_progress),
                visual.tint,
            ),
            color_b: tinted(
                ba::trail::evaluate_unity_trail_color(to_progress),
                visual.tint,
            ),
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
            progress: [1.0 - from_progress, -(to_progress - from_progress)],
            axis: [tangent.0, tangent.1, normal.0, normal.1],
            origin: [from.x - quad.loc.x as f32, from.y - quad.loc.y as f32],
            seg_end: local(to),
            extent: [length + 2.0 * joint_px as f32, width],
            // 首/尾段标记，保留给 `u_cap`。光晕现在**不再**用它收口：距离场本身
            // 在笔画端点外就是平滑的球形衰减，再按端点挖一个圆洞会把笔画中心挖成
            // 暗谷（详见 `cursor_effect.frag` 里 `glow_shape` 的说明）。
            cap: [
                if start == 0 { 1.0 } else { 0.0 },
                if end == last_point { 1.0 } else { 0.0 },
            ],
            ref_scale,
            glow_gain: visual.glow_gain,
            trail_glow_gain: visual.glow_trail_gain,
            glow_pass: false,
            glow_radius: visual.glow_radius,
            ring_inner: visual.ring_inner_ratio,
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

/// 元素内部的两种实现。
///
/// 绝大多数图元是「一次 draw 画一个四边形」的着色器元素；拖尾是个例外 ——
/// 它必须先以 `GL_MAX` 混合画进离屏纹理、再整体合成一次（见 [`TrailTarget`]），
/// 所以那边拿出来的是已经合成好的纹理元素。
#[derive(Debug, Clone)]
enum CursorEffectInner {
    Shader(ShaderRenderElement),
    Texture(TrailComposite),
}

/// 一个光标特效图元的渲染元素（与 `BorderRenderElement` 同模板）。
#[derive(Debug, Clone)]
pub struct CursorEffectElement {
    inner: CursorEffectInner,
    /// 该图元所属材质。仅供离线工具按材质隔离渲染 / 统计，不参与绘制。
    material: Option<Material>,
}

/// 把「两个内部实现同构」这件事写一次：两种类型实现了同一组 trait，且方法
/// 签名完全一致，所以函数体里换成哪个名字都一样。
macro_rules! delegate {
    ($this:expr, $elem:ident => $body:expr) => {
        match &$this.inner {
            CursorEffectInner::Shader($elem) => $body,
            CursorEffectInner::Texture($elem) => $body,
        }
    };
}

impl CursorEffectElement {
    /// 该图元所属材质（空图元为 `None`）。
    pub fn material(&self) -> Option<Material> {
        self.material
    }

    /// 组装一个轴对齐四边形并绑定 uniform / 纹理。`scale` 用来把物理像素换算回
    /// 逻辑坐标（smithay 的元素尺寸是逻辑像素）。
    fn build(command: &DrawCommand, scale: f32, texture: &GlesTexture) -> Self {
        Self {
            inner: CursorEffectInner::Shader(shader_element(command, scale, texture)),
            material: Some(command.material),
        }
    }
}

/// 构造「一次 draw 画一个四边形」的着色器元素。
fn shader_element(
    command: &DrawCommand,
    scale: f32,
    texture: &GlesTexture,
) -> ShaderRenderElement {
    {
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
            return ShaderRenderElement::empty(ProgramType::CursorEffect, Kind::Unspecified);
        }

        let input_to_geo = Mat3::from_scale(Vec2::new(
            command.quad.size.w as f32,
            command.quad.size.h as f32,
        ));
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
            Uniform::new("u_seg_end", command.seg_end),
            Uniform::new("u_extent", command.extent),
            Uniform::new("u_cap", command.cap),
            Uniform::new("u_bloom", BLOOM_GAIN * command.glow_gain * command.trail_glow_gain),
            Uniform::new("u_glow_radius", command.glow_radius),
            // 0 = 本体（预乘 over），1 = 辉光（`alpha = 0`，等效 `One/One` 加法）。
            Uniform::new("u_pass", if command.glow_pass { 1.0f32 } else { 0.0 }),
            Uniform::new("u_ring_inner", command.ring_inner),
            Uniform::new("u_ref_scale", command.ref_scale),
        ]);

        let textures =
            HashMap::from([(command.material.texture_name().to_string(), texture.clone())]);
        let mut inner = ShaderRenderElement::empty(ProgramType::CursorEffect, Kind::Unspecified);
        inner.update(size, None, scale, 1.0, uniforms, textures);
        inner = inner.with_location(loc);
        inner
    }
}

impl Element for CursorEffectElement {
    fn id(&self) -> &Id {
        delegate!(self, e => e.id())
    }
    fn current_commit(&self) -> CommitCounter {
        delegate!(self, e => e.current_commit())
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        delegate!(self, e => e.geometry(scale))
    }
    fn transform(&self) -> Transform {
        delegate!(self, e => e.transform())
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        delegate!(self, e => e.src())
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        delegate!(self, e => e.damage_since(scale, commit))
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        delegate!(self, e => e.opaque_regions(scale))
    }
    fn alpha(&self) -> f32 {
        delegate!(self, e => e.alpha())
    }
    fn kind(&self) -> Kind {
        delegate!(self, e => e.kind())
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
            // 离屏纹理里的内容也是同一套预乘值，直接贴回来即可。
            delegate!(self, e => RenderElement::<GlesRenderer>::draw(
                e, frame, src, dst, damage, opaque_regions, cache
            ))
        })
    }
    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        delegate!(self, e => e.underlying_storage(renderer))
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
        delegate!(self, e => e.underlying_storage(renderer))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 拖尾：离屏 `GL_MAX` 并集
// ─────────────────────────────────────────────────────────────────────────────

/// 拖尾的离屏合成目标。
///
/// 各段的光晕是解析近似，必须按「并集」而不是「叠加」合成。同一像素被 N 个
/// 四边形覆盖时，正确值是 `max_i halo(d_i)`；用预乘 over 合成得到的是
/// `1 - Π(1 - a_i)` —— 两者只在单层时相同。快速拖动时四边形重叠数按段长周期
/// 变化，over 会在转角叠出硬边亮斑；而旧实现「谁更近谁独占」的归属判据只能
/// 看 ±2 段，折返的两臂之间于是留下楔形缺口。
///
/// 这里把每段的 `(emission, coverage)` 用 `GL_MAX` 画进一张离屏纹理再整体合成
/// 一次。`max` 对段集合满足并集律，且
/// `max_i exp(-d_i / σ) ≡ exp(-min_i d_i / σ)` —— 取最大恰好等于折线距离场，
/// 是恒等而不是近似。于是重叠不叠亮、缺口自动补齐、远处折返回来的段也一并
/// 计入，`owns` / 邻居窗口那套几何补丁可以整个删掉。
///
/// 纹理只增不减地缓存：拖尾包围盒每帧都在变，按帧重建会稳定地每帧一次分配。
#[derive(Debug, Default)]
pub struct TrailTarget {
    cached: Option<CachedTarget>,
    /// `GL_MAX` 混合方程是否可用（GLES3 核心 / `EXT_blend_minmax`），只探测一次。
    max_blend: Option<bool>,
}

#[derive(Debug)]
struct CachedTarget {
    texture: GlesTexture,
    context_id: ContextId<GlesTexture>,
}

impl TrailTarget {
    /// 把一组拖尾段合成为一张纹理元素。
    ///
    /// `bounds` 是这些段的并集包围盒（输出物理像素），`elements` 必须是**相对
    /// `bounds.loc`** 构建的。任何一步失败都返回 `None`：拖尾是装饰，不该让
    /// 整帧渲染失败。
    pub fn composite(
        &mut self,
        renderer: &mut GlesRenderer,
        scale: f32,
        bounds: Rectangle<i32, Physical>,
        elements: &[ShaderRenderElement],
    ) -> Option<CursorEffectElement> {
        if bounds.size.w <= 0 || bounds.size.h <= 0 || elements.is_empty() {
            return None;
        }

        // 纹理只增不减，尺寸不够或渲染上下文变了才重建。
        let recreate = match &self.cached {
            Some(cached) => {
                cached.context_id != renderer.context_id()
                    || cached.texture.size().w < bounds.size.w
                    || cached.texture.size().h < bounds.size.h
            }
            None => true,
        };
        if recreate {
            let buffer_size = bounds.size.to_logical(1).to_buffer(1, Transform::Normal);
            let texture = match renderer.create_buffer(Fourcc::Abgr8888, buffer_size) {
                Ok(texture) => texture,
                Err(err) => {
                    tracing::warn!("cursor-effect: creating trail target failed: {err}");
                    return None;
                }
            };
            self.cached = Some(CachedTarget {
                texture,
                context_id: renderer.context_id(),
            });
        }
        let cached = self.cached.as_mut()?;

        let max_blend = *self
            .max_blend
            .get_or_insert_with(|| probe_max_blend(renderer));
        if max_blend {
            // smithay 每画一个图元都会重设 `glBlendFunc`，但**从不碰**
            // `glBlendEquation` —— 所以这样设一次就能跨过整个 draw 循环。
            let _ = renderer.with_context(|gl| unsafe { gl.BlendEquation(ffi::MAX) });
        }

        let result = draw_into_target(renderer, &mut cached.texture, scale, elements);

        if max_blend {
            let _ = renderer.with_context(|gl| unsafe { gl.BlendEquation(ffi::FUNC_ADD) });
        }
        if let Err(err) = result {
            tracing::warn!("cursor-effect: offscreen trail failed: {err:?}");
            return None;
        }

        Some(CursorEffectElement {
            inner: CursorEffectInner::Texture(TrailComposite {
                id: Id::new(),
                texture: cached.texture.clone(),
                context_id: cached.context_id.clone(),
                geo: bounds,
            }),
            material: Some(Material::Trail),
        })
    }
}

/// 探测 `GL_MAX` 混合方程。
///
/// 它是 GLES3 的核心能力；GLES2 上要靠 `EXT_blend_minmax`。拿不到时会返回
/// `GL_INVALID_ENUM`，这时退回普通的 over 合成 —— 关节处会有窄带叠亮，但
/// 画面不会坏。
fn probe_max_blend(renderer: &mut GlesRenderer) -> bool {
    let supported = renderer
        .with_context(|gl| unsafe {
            // 先吃掉历史错误，免得把别人的错算到自己头上。
            while gl.GetError() != ffi::NO_ERROR {}
            gl.BlendEquation(ffi::MAX);
            let error = gl.GetError();
            gl.BlendEquation(ffi::FUNC_ADD);
            error == ffi::NO_ERROR
        })
        .unwrap_or(false);
    if ce_dump_enabled() {
        eprintln!("  GL_MAX blend equation supported = {supported}");
    }
    if !supported {
        tracing::warn!(
            "cursor-effect: GL_MAX blend equation unavailable, \
             trail glow falls back to over-blending"
        );
    }
    supported
}

/// 清空离屏缓冲并把所有段画进去。
///
/// 每帧都必须**全量清除**：`GL_MAX` 只增不减，残留上一帧的亮值会永久留在
/// 纹理里。所以这里不用 `OutputDamageTracker` 的增量机制，直接清整个目标。
fn draw_into_target(
    renderer: &mut GlesRenderer,
    texture: &mut GlesTexture,
    scale: f32,
    elements: &[ShaderRenderElement],
) -> Result<(), GlesError> {
    if ce_dump_enabled() {
        let mut eq = [0i32; 1];
        renderer
            .with_context(|gl| unsafe {
                gl.GetIntegerv(ffi::BLEND_EQUATION_RGB, eq.as_mut_ptr());
            })
            .ok();
        eprintln!("  offscreen blend equation = 0x{:x}", eq[0]);
    }
    let texture_size = texture.size().to_logical(1, Transform::Normal).to_physical(1);
    let scale = Scale::from(scale.max(f32::EPSILON) as f64);
    let mut target = renderer.bind(texture)?;
    {
        let mut frame = renderer.render(&mut target, texture_size, Transform::Normal)?;
        frame.clear(
            Color32F::TRANSPARENT,
            &[Rectangle::from_size(texture_size)],
        )?;
        for element in elements {
            let dst = element.geometry(scale);
            if dst.size.w <= 0 || dst.size.h <= 0 {
                continue;
            }
            // 元素只覆盖自己那一段；越出纹理范围的部分由 GL 直接裁掉。
            let damage = [Rectangle::from_size(dst.size)];
            RenderElement::<GlesRenderer>::draw(
                element,
                &mut frame,
                element.src(),
                dst,
                &damage,
                &[],
                None,
            )?;
        }
        // 纹理随后在同一个命令流里被采样，不需要等 fence。
        let _ = frame.finish()?;
    }
    Ok(())
}

/// 已经合成好的拖尾纹理（整体贴回输出一次）。
#[derive(Debug, Clone)]
struct TrailComposite {
    id: Id,
    texture: GlesTexture,
    context_id: ContextId<GlesTexture>,
    /// 这张纹理在输出上的位置与尺寸（物理像素）。
    geo: Rectangle<i32, Physical>,
}

impl Element for TrailComposite {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        // 内容每帧整体重画，没有「局部提交」的概念。
        CommitCounter::default()
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geo
    }
    fn transform(&self) -> Transform {
        Transform::Normal
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((
            self.geo.size.w as f64,
            self.geo.size.h as f64,
        )))
    }
    fn damage_since(
        &self,
        _scale: Scale<f64>,
        _commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // 每帧整块重画，直接报整个包围盒。
        DamageSet::from_slice(&[self.geo])
    }
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }
    fn alpha(&self) -> f32 {
        1.0
    }
    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for TrailComposite {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        if frame.context_id() != self.context_id {
            tracing::warn!("cursor-effect: trail target belongs to another renderer");
            return Ok(());
        }
        // 纹理里已经是预乘好的 (emission, coverage)，贴回来走 smithay 默认的
        // `ONE, ONE_MINUS_SRC_ALPHA`，与逐段直接绘制是同一套数学。
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            1.0,
            None,
            &[],
        )
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for TrailComposite {
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
        _renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// 收集一个输出上需要绘制的光标特效元素。
///
/// `Snapshot` 里的坐标已经是「相对该输出左上角」的物理像素（模拟的视口就是输出
/// 的物理尺寸），所以这里不需要输出在全局空间里的位置。
///
/// # 返回顺序：**从前到后**（栈顶在前），不是绘制顺序
///
/// niri 的 `push` 约定是「先推的在上层」——`render_to_vec`、`render_to_texture`、
/// 录屏与截图全部在真正绘制时 `.iter().rev()`，所以最后推出的那个元素最先落笔、
/// 处于最底层（见 `render_inner` 里 pointer 第一个 push、注释却写着 "The pointer
/// goes on the top"）。
///
/// 这里因此把 [`build_commands`] 的绘制顺序（拖尾 → 环雾 → 环 → 盘 → 盘雾 → 碎片）
/// 反过来再返回。曾经漏了这一步，实机上整条特效被上下颠倒：碎片被压到最底、
/// 拖尾的辉光反而盖在最上面把盘洗白，而离线渲染器（直接按返回顺序画）看不出这个
/// 问题——同一份代码，两种顺序，正是「实机和渲染图不相符」的根因。
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

    let commands = build_commands(&snapshot, state.visual());
    tracing::debug!(
        "cursor-effect: {} commands (sprites={} strokes={} ref_scale={:.3} viewport={:?})",
        commands.len(),
        snapshot.sprites.len(),
        snapshot.trail_strokes.len(),
        snapshot.ref_scale,
        state.viewport()
    );

    // `CE_CMDDBG=1` 时把每帧的 DrawCommand 参数打到 stderr，用来核对「图元在屏幕上
    // 到底多大、辉光半径倍率是多少」——标定辉光指数时离不开这两个数（`ring_out`
    // 是像素口径，没有它就只能靠猜）。环境变量只在首次调用时读一次，热路径上不再
    // 有 `getenv`。
    static CMDDBG: OnceLock<bool> = OnceLock::new();
    if *CMDDBG.get_or_init(|| std::env::var_os("CE_CMDDBG").is_some()) {
        eprintln!(
            "CMDBG ref_scale={:.4} viewport={:?}",
            snapshot.ref_scale,
            state.viewport()
        );
        for c in &commands {
            eprintln!(
                "  {:?} quad=({:.0},{:.0} {:.0}x{:.0}) center={:?} size={:.2} \
                 glow_radius={:.3} intensity={:.3} gop={:.3} color_a={:.3}",
                c.material,
                c.quad.loc.x,
                c.quad.loc.y,
                c.quad.size.w,
                c.quad.size.h,
                c.center,
                c.size,
                c.glow_radius,
                c.intensity,
                c.gop,
                c.color_a
            );
        }
    }

    // 拖尾先画（它在 `build_commands` 里也排在最前）：整条笔画合成成**一个**
    // 离屏元素，后面的点击图元再叠上去。
    let mut out = Vec::with_capacity(commands.len());
    if let Some(element) = build_trail_element(renderer, state, &commands, scale) {
        out.push(element);
    }
    for command in &commands {
        if command.material == Material::Trail {
            continue;
        }
        tracing::debug!(
            "cursor-effect: {:?} quad=({},{} {}x{})",
            command.material,
            command.quad.loc.x,
            command.quad.loc.y,
            command.quad.size.w,
            command.quad.size.h
        );
        out.push(CursorEffectElement::build(
            command,
            scale,
            textures.get(command.material),
        ));
    }

    // 绘制顺序 → niri 的 push 顺序（从前到后）。见函数文档：调用方在落笔前会
    // 反向迭代，这里不翻转的话整条特效会被上下颠倒。
    out.reverse();
    out
}

/// 把本帧所有拖尾段合成成一个离屏元素。
fn build_trail_element(
    renderer: &mut impl NiriRenderer,
    state: &CursorEffectState,
    commands: &[DrawCommand],
    scale: f32,
) -> Option<CursorEffectElement> {
    let Some(textures) = Shaders::get(renderer).cursor_effect_textures.clone() else {
        return None;
    };

    // `CE_SEG=n`：只合成第 n 段。离线诊断用 —— 把每段单独渲染出来，逐像素
    // 取最大值应当等于整条笔画的渲染结果，这就是「`GL_MAX` 等价于并集」的直接
    // 验证（见 `work/verify_union.py` 一类的脚本）。
    let only_segment = ce_only_segment();
    let trail: Vec<&DrawCommand> = commands
        .iter()
        .filter(|c| c.material == Material::Trail)
        .enumerate()
        .filter(|(index, _)| only_segment.map(|n| n == *index).unwrap_or(true))
        .map(|(_, command)| command)
        .collect();
    if trail.is_empty() {
        return None;
    }

    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for command in &trail {
        min_x = min_x.min(command.quad.loc.x);
        min_y = min_y.min(command.quad.loc.y);
        max_x = max_x.max(command.quad.loc.x + command.quad.size.w);
        max_y = max_y.max(command.quad.loc.y + command.quad.size.h);
    }

    // CE_DUMP=1：离线诊断用，把拖尾命令的几何与着色输入原样打印。
    if ce_dump_enabled() {
        for command in &trail {
            println!(
                "    TrailCmd origin=({:.1},{:.1}) seg_end=({:.1},{:.1}) prog=({:.4},{:.4}) cov=({:.4},{:.4}) color=({:.3},{:.3},{:.3}) color_b=({:.3},{:.3},{:.3}) a={:.3} extent=({:.1},{:.1}) size={:.2} glow_r={:.3}",
                command.quad.loc.x + command.origin[0] as f64,
                command.quad.loc.y + command.origin[1] as f64,
                command.quad.loc.x + command.seg_end[0] as f64,
                command.quad.loc.y + command.seg_end[1] as f64,
                command.progress[0],
                command.progress[1],
                command.cov[0],
                command.cov[1],
                command.color[0],
                command.color[1],
                command.color[2],
                command.color_b[0],
                command.color_b[1],
                command.color_b[2],
                command.color_a,
                command.extent[0],
                command.extent[1],
                command.size,
                command.glow_radius,
            );
        }
    }

    // 裁到视口：输出外的部分本来也看不见，不裁会让整条笔画都去申请纹理。
    let viewport = state.viewport();
    let viewport_rect = Rectangle::new(
        Point::from((0, 0)),
        Size::from((viewport.width as i32, viewport.height as i32)),
    );
    let spanned = Rectangle::new(
        Point::from((min_x.floor() as i32, min_y.floor() as i32)),
        Size::from((
            (max_x.ceil() as i32) - (min_x.floor() as i32),
            (max_y.ceil() as i32) - (min_y.floor() as i32),
        )),
    );
    let bounds = viewport_rect.intersection(spanned)?;

    // 所有四边形平移到以 `bounds.loc` 为原点 —— 离屏纹理就是 `bounds` 这么大。
    let elements: Vec<ShaderRenderElement> = trail
        .iter()
        .map(|command| {
            let mut local = **command;
            local.quad.loc = Point::from((
                command.quad.loc.x - bounds.loc.x as f64,
                command.quad.loc.y - bounds.loc.y as f64,
            ));
            shader_element(&local, scale, textures.get(Material::Trail))
        })
        .collect();

    state
        .trail_target()
        .borrow_mut()
        .composite(renderer.as_gles_renderer(), scale, bounds, &elements)
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
    fn curve_draw_refines_the_polyline() {
        use ba::TrailPoint;
        // 一段 300px 的直路被 40Hz 采样成 3 个点（间隔 150px）。
        let pts = vec![
            TrailPoint { position: ba::Point::new(0.0, 0.0), normalized_age: 0.0 },
            TrailPoint { position: ba::Point::new(150.0, 10.0), normalized_age: 0.5 },
            TrailPoint { position: ba::Point::new(300.0, 0.0), normalized_age: 1.0 },
        ];
        let refined = resample_curve(&pts);
        // 8px 步长 ⇒ 每段补 19 个点左右，总量远多于原样。
        assert!(refined.len() > 30, "len={}", refined.len());
        // 首尾不动。
        assert_eq!(refined.first().unwrap().position, pts[0].position);
        assert_eq!(refined.last().unwrap().position, pts[2].position);
        // 曲线经过中间采样点本身。
        assert!(refined
            .iter()
            .any(|p| (p.position.x - 150.0).abs() < 1e-4 && (p.position.y - 10.0).abs() < 1e-4));
        // 单调性：弧长沿 x 单调递增（没有过冲翻折）。
        for pair in refined.windows(2) {
            assert!(pair[1].position.x >= pair[0].position.x - 1e-3);
        }
        // 关掉时原样返回。
        assert_eq!(resample_curve(&pts[..1]).len(), 1);
    }

    #[test]
    fn click_draws_disk_then_ring_then_shards() {
        let snapshot = click_snapshot();
        let commands = build_commands(&snapshot, &ba::VisualConfig::default());
        assert!(commands.len() >= 2);
        // 雾全部铺在最底。
        assert!(
            commands[0].glow_pass,
            "首条命令必须是辉光遍（雾铺底），实际是 {:?}",
            commands[0].material
        );
        // 本体（`glow_pass == false`）之间的顺序才是层序；辉光遍统一在开头重排。
        let body = |material: Material| {
            commands
                .iter()
                .position(|c| c.material == material && !c.glow_pass)
                .unwrap_or_else(|| panic!("{material:?} 本体"))
        };
        // 上游提交顺序：Cross2 盘 → 同队列 4499 的 dissolve ring → Tri2(4550)。
        // 环必须压在盘上面，否则环生长到盘内那段时间（0~200ms）会被盘整块盖掉，
        // 点击瞬间就看不到白色光环。
        assert!(body(Material::Disk) < body(Material::Ring), "圆盘先于圆环绘制");
        // 三角形（队列 4550）永远在圆盘/圆环（4499）之后，也就是最上层。
        if commands.iter().any(|c| c.material == Material::Triangle) {
            assert!(body(Material::Triangle) > body(Material::Ring));
        }
        // 环的雾夹在盘本体与环本体之间；其余雾一律在本体之前铺底。
        let first_body = commands
            .iter()
            .position(|c| !c.glow_pass)
            .expect("至少有一个本体");
        for (idx, cmd) in commands.iter().enumerate() {
            if cmd.glow_pass {
                if cmd.material == Material::Ring {
                    assert!(
                        idx > body(Material::Disk) && idx < body(Material::Ring),
                        "环的雾必须夹在盘本体与环本体之间，否则盘面那圈内光会丢"
                    );
                } else {
                    assert!(
                        idx < first_body,
                        "{:?} 的辉光遍必须在本体之前，否则会糊在本体表面（漏光）",
                        cmd.material
                    );
                }
            }
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
            glow_gain: 1.0,
            trail_glow_gain: 1.0,
            glow_radius: 1.0,
            ring_inner: 0.940_127_5,
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
        // 45° 旋转后包围盒边长 = 40 * (sin + cos) * 辉光放大倍数 + padding（单边）
        // + 屏幕空间留白（碎片的雾沿三角形 SDF 衰减，同样要外扩）。
        let expected = 40.0 * std::f32::consts::SQRT_2 as f64 * BLOOM_QUAD_SCALE
            + QUAD_PADDING * 2.0
            + TRIANGLE_HALO_PX * 2.0;
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
                glow_gain: 1.0,
                trail_glow_gain: 1.0,
                glow_radius: 1.0,
                ring_inner: 0.940_127_5,
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
            let need =
                size as f64 * 0.5 * BLOOM_QUAD_SCALE + QUAD_PADDING + DISK_HALO_PX * scale as f64;
            assert!(
                (command.quad.size.w * 0.5 - need).abs() < 1e-3,
                "高度 {height}：圆盘四边形 {:.1} 装不下 {need:.1} 的光晕留白",
                command.quad.size.w * 0.5
            );
        }

        // 环的外晕也是屏幕空间的（环带只有 6% 宽，雾全靠 `RING_GLOW_*` 那两根
        // 指数），所以它同样要按像素外扩；半径越大、雾越长，越不能靠倍数留白。
        for height in [1080_u32, 1440] {
            let scale = ba::reference_scale(ba::Viewport::new(1920, height));
            let base = Base {
                color: [1.0; 3],
                color_a: 1.0,
                gop: 1.0,
                intensity: 5.99,
                threshold: 0.0,
                ref_scale: scale,
                glow_gain: 1.0,
                trail_glow_gain: 1.0,
                glow_radius: 1.0,
                ring_inner: 0.940_127_5,
            };
            let size = 0.15 * height as f32;
            let ring = DrawCommand::sprite(
                Material::Ring,
                ba::Point::new(500.0, 500.0),
                size,
                0.0,
                true,
                [0.0, 1.0],
                &base,
            );
            let need =
                size as f64 * 0.5 * BLOOM_QUAD_SCALE + QUAD_PADDING + RING_HALO_PX * scale as f64;
            assert!(
                (ring.quad.size.w * 0.5 - need).abs() < 1e-3,
                "高度 {height}：环四边形 {:.1} 装不下 {need:.1} 的光晕留白",
                ring.quad.size.w * 0.5
            );
        }

        // 碎片也要外扩：碎片的雾沿三角形 SDF 衰减，留白为 0 时那圈光会被四边形
        // 切成方块。只是像素数比盘/环小（64 而非 190），免得填充率吃亏。
        let base = Base {
            color: [1.0; 3],
            color_a: 1.0,
            gop: 1.0,
            intensity: 1.0,
            threshold: 0.0,
            ref_scale: 1.0,
            glow_gain: 1.0,
            trail_glow_gain: 1.0,
            glow_radius: 1.0,
            ring_inner: 0.940_127_5,
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
        let expected = 40.0 * BLOOM_QUAD_SCALE + QUAD_PADDING * 2.0 + TRIANGLE_HALO_PX * 2.0;
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

            let commands = build_commands(&snapshot, &ba::VisualConfig::default());
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
        let commands: Vec<DrawCommand> = build_commands(&snapshot, &ba::VisualConfig::default())
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
