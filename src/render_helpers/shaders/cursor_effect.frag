precision highp float;

#if defined(DEBUG_FLAGS)
uniform float niri_tint;
#endif

uniform float niri_alpha;
uniform float niri_scale;
uniform vec2 niri_size;
varying vec2 niri_v_coords;

// ─────────────────────────────────────────────────────────────────────────────
// BA 点击特效材质（移植自 ba-click-fx-desktop 的 `fxMaterialsShaderSource`）
//
// 逐个材质对齐上游：
// · `CrossPixel`     → 圆盘（`u_mode == 0`）
// · `DissolvePixel`  → 溶解环（`u_mode == 1`）
// · `AdditivePixel`  → 三角碎片（`u_mode == 2`）
// · `TrailPixel`     → 拖尾（`u_mode == 3`）
//
// 色彩：上游把四张材质纹理按 sRGB SRV 采样（`R8G8B8A8_UNORM_SRGB`），所以
// `srgb_to_linear(texel.rgb)` 才是它们进入线性域的值；而精灵/拖尾的顶点色在
// Unity 里本身就是线性域作者色，**不能再转一次**。最终上游把 direct 与 bloom
// 在线性域相加后用 `LinearToSrgb` 编码，再用 `capacityScale` 把预乘色压到不
// 超过 coverage（见 `ResolveUnknownBackgroundDesktopTransport`）。niri 直接输
// 出到 sRGB 编码的合成目标，所以这里复刻「编码 + 容量压缩」，把结果当作预乘
// sRGB 交给 smithay 默认的 `ONE, ONE_MINUS_SRC_ALPHA` 混合。
//
// 几何：四边形永远是轴对齐的，`input_to_geo` 只是把 `niri_v_coords ∈ [0,1]²`
// 放大成「相对四边形左上角的像素」。精灵靠 `u_center` / `u_rot` / `u_size` 在
// 着色器里反旋，拖尾段靠 `u_origin` / `u_axis` / `u_extent` 投影到条带坐标系。
// 因此四边形被刻意画得比图元本身大一圈（见 `render.rs` 的 padding 常量），好让
// 辉光能溢出形状边缘。
//
// 辉光（bloom）：上游是独立的 mip 链后处理（`unity_bloom.cpp`，diffusion 7、
// intensity 1.7 ⇒ `exposureGain ≈ 0.125`，再叠加各级上采样），没法在 niri 的
// 「一个元素一次 draw」管线里做同构实现，这里改成解析近似：把图元的形状当作解
// 析几何，用「高斯模糊后的实心区域」函数求它在自己形状外的扩散，再乘 `u_bloom`
// 增益。增益与扩散半径按上游官方演示图（`docs/images/native-click-trail.png`）
// 的径向剖面标定：形状内部 ≈ 直射色的 50%，形状外以 σ ≈ 0.45 半尺寸衰减。
// ─────────────────────────────────────────────────────────────────────────────

uniform mat3 input_to_geo;

uniform float u_mode;      // 0=圆盘 1=溶解环 2=三角碎片 3=拖尾
uniform vec3 u_color;      // 主色（线性域作者色）
uniform vec3 u_color_b;    // 拖尾：段尾颜色（线性域）
uniform float u_color_a;   // 顶点色 alpha
uniform float u_gop;       // 全局不透明度
uniform float u_intensity; // Unity HDR 强度
uniform float u_threshold; // 溶解裁剪阈值
uniform vec2 u_center;     // 精灵中心（相对四边形左上角的像素）
uniform float u_size;      // 精灵边长（像素）
uniform float u_rot;       // 精灵/环旋转（弧度）
uniform vec2 u_uv_off;     // 图集 u：起点与跨度
uniform vec2 u_cov;        // 拖尾：两端纵向覆盖
uniform vec2 u_progress;   // 拖尾：纹理 u 起点与增量
uniform vec4 u_axis;       // 拖尾：切线 xy、法线 zw（单位向量）
uniform vec2 u_origin;     // 拖尾：段起点（相对四边形左上角的像素）
/// 拖尾：段末端（相对四边形左上角的像素）。
///
/// 由 Rust 侧直接给出**原始端点**，不用 `u_origin + u_axis * seg_len` 反算 ——
/// 反算会引入 1e-3 量级的浮点差，在等距线上表现为放射状暗带。
uniform vec2 u_seg_end;
uniform vec2 u_extent;     // 拖尾：段长与带宽（像素）
uniform vec2 u_cap;        // 拖尾：首/尾段的纵向渐隐长度（像素，0 = 硬切）
uniform float u_bloom;     // 辉光增益（0 = 关闭）
uniform float u_glow_radius; // 辉光半径倍率（1 = 上游原尺寸）
uniform float u_ring_inner;  // 溶解环内径比（外径的倍数）
/// 渲染遍：0 = 本体（预乘 over），1 = 辉光（纯加法）。
///
/// 上游把两者放在**两张 RT** 里：本体走 emission pass，辉光走 prefilter → mip
/// 链 → `One/One` 加回 Scene。合成方式不同是本质区别 ——
///
/// - 预乘 over：`dst = src + dst × (1 - src.a)`，雾会**压暗背景**。雾越亮，背景
///   被吃掉越多，于是环带两侧的层次被抹平，环看起来「糊成一团」。之前所有版本
///   都是这样：把 `bloom_coverage` 掺进 `alpha`，雾就变成了一层实心亮膜。
/// - `One/One` 加法：`dst = src + dst`，雾只**提亮**、不遮挡，背景原样透出来，
///   本体边缘的对比度一分不减。
///
/// 我们没法给单个图元换 blend func（smithay 每个图元都重设 `glBlendFunc`），但
/// 有个等价写法：预乘 over 下 `src.a = 0` 时 `dst = src.rgb + dst × 1`，正好就是
/// 加法。所以辉光遍只要输出 `alpha = 0` 即可，不需要碰混合状态。
uniform float u_pass;

/// 输出高度相对 1080p 的倍率。
///
/// **不能**用 `niri_size`：smithay 交给着色器的 `niri_size` 是四边形自身的尺寸，
/// 而辉光半径是屏幕空间量（上游是整帧图像上的 mip 链）。用四边形尺寸做归一化，
/// 光晕会缩到原来的几十分之一 —— 在离线 harness 里看不出来（那里 `niri_size`
/// 恰好等于输出尺寸），一到实机就完全没有辉光。
uniform float u_ref_scale;

uniform sampler2D tex_disk;
uniform sampler2D tex_ring;
uniform sampler2D tex_triangle;
uniform sampler2D tex_trail;

const float PI = 3.141592653589793;
/// `makeUnityRingMesh()` 的 `firstAngleRadians = 95.625°`。
const float RING_FIRST_ANGLE = 1.6689711;
/// `unityRingInnerRadius / unityRingOuterRadius`。
const float RING_INNER_RATIO = 0.9401275;
/// 环带的**视觉**加厚量（从 `inner` 再往里吃掉的半径比例）。
///
/// 上游 `0.9401275` 只给 6% 的带宽：环外半径 80.41px 时整条带子只有 4.8px，
/// 抠掉抗锯齿的 1px 羽化，真正「实心白」的部分不到 3px —— 实机上就是一根
/// 几乎看不见的细线，而参考帧那道环在截图里明显是 6~8px 的厚带。
///
/// 纹理的径向 alpha 剖面本身是平的（227…255…231，只在两端各掉 10%），所以
/// 加宽不会把环带拉灰，只是把同一段渐变摊到更宽的径向区间上。0.03 相当于
/// inner 0.9401 → 0.9101、带宽 6% → 9%，在 1080p 参考系里是 4.8px → 7.2px。
const float RING_THICKEN = 0.03;
/// 环带**内外两条边**的径向羽化半宽（1080p 参考像素）。
///
/// 环带原来只有 `aa_coverage` 的 1px 抗锯齿：1600p 下剖面是 111 → 199 → 206
/// → 206 → 204 → 199 → 111，一步到位，读起来是「一根实心塑料管」。
/// 参考帧 `17-27-14` 同一位置是 196 / 254 / 185 / 113 —— 过渡跨 5~6px，
/// 中心最亮、两侧各有一半的渐变，这才是「发光带」的剖面。
///
/// 2.2px 是把参考那条斜坡反解出来的半宽：环带本身 7px 宽，两侧各吃掉 2px
/// 之后中间还剩 3px 实心核，正好对上参考里那条 3px 的白线。
const float RING_EDGE_SOFTNESS = 2.2;
/// 网格 uv 留出的半像素边距（`firstU/lastU/innerV/outerV` 取 0.0005..0.9995）。
const float RING_UV_INSET = 0.0005;
const float RING_UV_SPAN = 0.999;

/// 圆盘纹理 `center_disk` 里那个圆片的归一化半径：r ≤ 225 恒为 255，到 r = 233
/// 衰减完，按 512 宽折算是 0.879。辉光的形状边界必须用这个值，而**不是 1.0**
/// —— 否则光晕会浮在实体外面一圈，盘和雾之间空出一道暗环。
const float DISK_SHAPE_RADIUS = 0.879;

/// 圆盘外晕的径向衰减（1080p 参考像素，从**盘边**起算）。
///
/// 上游是整帧 mip 链 bloom：远场的衰减长度会随尺度递增，单条指数撑不出「近处
/// 紧、远处铺一片淡蓝」的形状，所以和拖尾一样用多段叠加。
///
/// 参数按 `docs/images/native-click-trail.png` 的盘外径向剖面拟合（先扣掉纯黑
/// 背景、再转到线性域）。**拟合区间从亮环外侧开始**：官方图在离盘边 0~6px 处
/// 那圈近白的 (226,239,255) 是两道溶解环本体，不是圆盘的辉光。早先把这段也算
/// 进圆盘，反解出一个 4px 的「白热核心」，结果盘边糊成一圈厚厚的白环 ——
/// 而真正的环反倒被盖住了。
///
/// 早期那组 (12 / 33, 增益 0.250 / 0.215) 是拿**未扣背景**的参考像素直接拟合的，
/// 而官方图是一张真实桌面截图（背景约 (5,8,20)，且不均匀）。扣掉局部背景后重扫，
/// 那组在中远场亮 2~5 倍 —— 这就是「整体颜色怪、没有蔚蓝发光」的来源：蓝雾太厚，
/// 把本该透出来的深色桌面糊成一片青。
///
/// 现按扣背景后的目标剖面重新扫描（见 `fitdisk.py`，4 组候选里误差最小的一组）：
/// 圆盘外晕改用上面的 mip 链核：6 级全开（σ 3…96），雾自己铺到 ~90px 外才
/// 收干净 —— 圆盘是画面里最大的一块实体，没理由比环先断。
const float DISK_HALO_SIGMA_MAX = 56.0;
/// 两段的增益。口径是 `glow_shape`：`bloom_linear` 还要再乘
/// `glow_source * u_bloom`，而圆盘固定是 `u_intensity = 2.4`、`u_bloom = 0.125`。
///
/// **辉光改走加法遍之后，这两支必须整体下调。** 旧值（0.200 / 0.026）是按
/// 「预乘 over」标定的：那时雾的 alpha 只有 0.9 上限，还要按 `(1 - 雾覆盖率)`
/// 压掉背景，等于自带一层衰减。改成 One/One 加法后雾是**全量相加、没有任何
/// 遮挡**，同样的增益会直接叠成一片暴亮的白盘 —— 实测就是「整个圆盘都在发光」。
/// 主瓣 τ 也在这一轮放长（8 → 13px）：τ 太短时雾在盘边 8px 内就衰完，加法下
/// 表现为盘沿一圈**硬亮边**，而不是参考图那种从盘边慢慢摊开的雾。
const float DISK_HALO_GAIN = 0.3000;
/// 外晕往白里提的比例。上游 bloom 的 `whiteCore` 补偿会把亮核的弱通道抬向峰值，
/// mip 链把这份「发白」摊到整片外晕，所以纯作者蓝喂不出官方那圈淡雾。
///
/// 但白度**不能给满**。0.29 时外晕三通道已经接近灰白，叠在亮环外侧就是一圈
/// 「肥厚白边」，观感很实、很闷；降到 0.18 之后雾仍然偏蓝，透出来的是背景色而
/// 不是白色，这才是「透明」的来源。压暗和降白度是两件独立的事，必须一起做 ——
/// 只压增益不降白度，得到的是「暗而脏」的灰雾。
const float DISK_HALO_WHITE_MIX = 0.06;
/// （`DISK_HALO_END` 截断窗已删。）
///
/// 曾经留了一个「把最后看不见的一截抹平」的 `1 - smoothstep(0.8E, E, d)` 窗，
/// 四边形留白正好压在窗终点上。但换成 mip 链核后最粗一级 σ=96 的高斯尾巴在
/// 88px 处还有峰值的 1.6%，窗在 88→110px 之间把它一刀压到 0 —— 实机截图里
/// 雾被一个圆弧形的暗边啃掉一块，就是这刀。高斯尾巴是 C∞ 的，让它自己收干，
/// 四边形放大到残差 <1/255 的位置（`DISK_HALO_PX` 190px，见 `render.rs`）。
/// 近场亮边（贴着形状 5~20px 的那层额外光）。
///
/// 单根 `bloom_mip` 的 `kernel(0)` 虽然恒等于 1，但它被**远场**标定过增益：
/// 主瓣增益压到 0.094 之后，贴边那几像素只能贡献 +30/255 左右，而形状本体是
/// 100~180/255。于是「本体 → 雾」之间不是渐变，而是**一级台阶**：盘沿外两三
/// 像素立刻掉到接近背景，看起来就是圆盘与外面那圈雾之间空了一条缝。
///
/// 参考图不是这样：白色环带外侧 2px 还有 231/255（扣背景后 193），5px 处
/// 166，说明贴边那一层的光比远场残尾强得多。所以补一支**短 σ**的分量，
/// 只负责贴边十几像素，远场形状交给原来那支。
/// σ 只能取到 3（`BLOOM_MIP_SIGMA0`）；再往上摊，1.05R 之后的远场会被一起
/// 抬起来 —— 实测 σ=6 时 1.08R 处比参考亮 1.7 倍。参考那条曲线在 1.02R 还有
/// 207/255、1.08R 只剩 110，四五个像素就衰完，所以这支必须**贴着边**。
/// **0（原 0.18）—— 参考帧的盘面到边是平的，没有贴边亮圈。**
///
/// 按拟合圆心（盘色区域质心 `(415.1, 300.4)`）重量的参考帧 `17-27-01`：
/// r=0..70 恒为 (106,167,255)，只有进入环带内沿（r≥72）才抬起来。而这一支
/// 在 1600p t=0.10 把盘的最后 6px 抬到 (156,213,255) —— 比盘心亮 45/53，
/// 合成后就是「盘沿焊了一圈白光」，环带还没开始就已经饱和。归一化剖面上
/// 我们 0.8R 处是参考的 2.09 倍、0.9R 处 5.13 倍，主要就是这支。
///
/// 盘本体 → 盘外雾之间的过渡由 `DISK_HALO_GAIN` 的远场核自己给
/// （σ 链到 96，`kernel(0)=1`，贴边本来就有 +30/255），不需要额外近场支。
const float DISK_EDGE_SIGMA = 3.0;
const float DISK_EDGE_GAIN = 0.0;
const float RING_EDGE_SIGMA = 3.0;
/// 环带的近场亮边。**这条 02:39 加的补光是为了补一个已经不存在的缝**。
///
/// 那时环整层排在盘**下面**，盘遮住环的内沿，看起来像「盘和雾之间空了一条」。
/// 层序改成跟着上游走（盘先画、环压盘上、雾最后统一叠加）之后，环就从盘边
/// 直接接了出来 —— 缝的来源没了，这 0.22 只剩下副作用：`kernel(0)=1` 再乘
/// 材质强度 5.99，贴边五六像素被它整片推到饱和，两条环带之间那 7px 的空隙
/// 也被填平，一条 6% 宽的细环带读成了 20% 宽的肥白圈（1600p 下实测饱和区
/// 57→79px，而环带本身只有 64.5→79.9px）。
///
/// 降到 0.08 仍然偏肥：这一遍是加法，环本体（强度 5.99）已经把环带推到
/// 接近饱和，贴边再 +0.08 就把饱和区从环带本身撑到两侧各多出 3~4px。
/// 参考图环带峰值 229、外侧 1.12R 处 183，都不是靠贴边补光撑出来的。
///
/// 现在本体已经被 `RING_EMISSION_SCALE` 压到 0.26、不再饱和，这支的职责回到
/// 它最初的意图：把环带外侧那圈「紧光」补上来。参考帧盘边对齐后 1.05R 处的
/// 线性富余是盘心的 3.52 倍，我们只有 1.24 —— 差的就是这一圈。0.16 是「让
/// 1.05R 追平、同时不把 1.10R 抬过头」的折中，改完要按同一张表复测。
const float RING_EDGE_GAIN = 0.0;

/// 环带内沿相对盘形状半径的位置（盘半径 = `DISK_SHAPE_RADIUS × half_px`）。
///
/// 盘的 shader 拿不到环的尺寸（每个精灵只知道自己的 `u_size`），而「补回环往
/// 洞里渗的那层雾」又必须知道环内沿在哪，所以这里用实测比值：环内沿一直在盘
/// 形状半径的 0.84~0.90 之间（`work/overlap.py` 逐帧量的），取 0.86。
const float DISK_RING_INNER_RATIO = 0.86;

/// 盘自己那层雾的增益（作用在**盘面之上**：这条命令排在盘本体之后，见
/// `render.rs` 的 `draw_rank`）。
///
/// 盘本体自己是一片**死平**的饱和蓝（实测整片 R 恒为 111，盘纹理没有径向渐
/// 变）。参考帧 `17-27-14` 那条朝环边提亮的曲线（R 由盘心 108 升到环边 151）
/// 在上游是 bloom 后处理压出来的，这里靠这支补光复现。
///
/// 形状用线性斜坡而不是高斯核：参考的爬升是**均匀摊在整个半径上**的，高斯核
/// 怎么调都只能给出「盘心死平 + 贴边陡升」，实机上就是盘里多出一圈亮环。
/// 最中心 12% 再补一个 `smoothstep` 收口，避免斜坡在盘心留一个尖点。
///
/// **已归零。** 这支的立论是「参考帧的盘面朝环边提亮」，但那条提亮其实是
/// **环自己的内侧雾**落在盘上，不是盘发的光。上游 `disk` 是纯 `CrossPixel`：
/// 径向能量 `textureRadialEnergyKeys` 在 0.84R 之前**恒为 1**，之后一路掉到 0
/// —— 盘内根本没有任何增亮项。
///
/// 留着它的代价是致命的：这一遍是**加法**，而环本体已经把盘沿推到接近饱和，
/// 再叠 0.128 就把整条环带连同内侧十几像素一起压到 255，于是「细白线 + 淡雾」
/// 读成一大圈肥厚白甜甜圈（1600p 实测 1.0R 附近 20px 全是 255，而参考同处
/// 峰值只有 229 且始终不饱和）。
const float DISK_HOLE_LEAK_GAIN = 0.0;

/// 盘面朝边的**自发光**：盘不是一块死平的色板，越靠边越亮、贴环那一圈最亮。
///
/// 走的是**盘本体自己的 `emission`**，走不了雾：
/// · 雾是 rank 0 的加法遍（`draw_rank`），盘本体是一层不透明的预乘 over，
///   会在盘内把雾整片扣掉 —— 盘面一个像素都拿不到。之前把盘的雾提到本体
///   之后，代价是整块盘上糊了一层半透明蓝膜，环带内侧的层次全被抹平。
/// · 直接往线性域加能量也没用：盘面 B 通道早就打在 255，加多少都只会抬高
///   `peak`，收尾的 `capacity = alpha / peak` 又按新峰值把整块盘等比压回去
///   （实测加 0.19 线性只换来 +1 级，甚至更暗）。
///
/// 所以做法是「**乘增益，再夹回原峰值**」：B 纹丝不动，R/G 追上来，盘边读作
/// 「亮到发白」。峰值不变 ⇒ `capacity` 是恒等变换 ⇒ 增益是实打实的。
///
/// 颜色口径是**编码域**的观测值（参考帧盘心 (110,169,255) → 0.97R (118,177,255)
/// 附近仍然不饱和），所以这里给的是「编码域看上去的亮度」，由 `emission`
/// 的线性域乘同一个系数近似 —— sRGB 幂函数在 0.15~0.6 这一段近似线性。
///
/// 起点放在 0.50R：参考帧 0.5R 以内盘面是**均匀蔚蓝**（实测 0→0.5R 恒在
/// ±2 级内），抬升只发生在最后半圈。
const float DISK_RIM_START = 0.75;
const float DISK_RIM_END = 1.00;
/// 贴边处的通道增益。1.32 对应盘心 (111,160,255) → 盘边 (126,181,255)，
/// 与参考帧同位置 (118,177,255) 差 3~8 级（方向上一致、幅度略强一点点）。
const float DISK_RIM_GAIN = 1.32;

/// 上游 `ResolveRecordingCompatibleDesktopTransport` 的预设参数：源覆盖 Alpha
/// 上限 0.90（给「网页透明叠加」预留背景透视），亮核补偿强度 0.35。
///
/// niri 这边两处都做了调整，理由见各自使用处：
/// · Alpha 上限随实体覆盖度放开（实体盘实心、光晕仍守 0.90）；
/// · 亮核补偿 0.35 → 0.10，否则圆盘会被抬成灰蓝色（见下方合成段注释）。
const float TRANSPORT_ALPHA_LIMIT = 0.90;
const float TRANSPORT_COMPENSATION_MIX = 0.10;

/// 抗锯齿半宽（物理像素）。niri 以 1:1 物理像素绘制本元素，`pixel` 的片元步长
/// 恒为 1 像素，所以可以直接用像素单位的常量做 SDF 羽化，不必依赖 `fwidth`
/// （GLES2 里需要 `GL_OES_standard_derivatives` 扩展）。
const float AA_HALF_WIDTH = 0.5;
/// 溶解阈值的软化半宽（Alpha 单位）。只影响阈值处的过渡，溶解形状基本不变。
const float DISSOLVE_SOFTNESS = 0.02;

/// 环的外晕（1080p 参考像素，从**环带边缘**起算 —— 外缘向外、内缘向洞内）。
///
/// 参考站 5 张 1:1 截图（`Screenshot from 2026-10-31 17-27-{01,08,11,14,19}`，
/// 环外半径 92px）沿径向取 90 分位亮度、扣背景后换回线性域，得到的是**非常
/// 陡**的一条曲线，而不是先前以为的「长尾巴」：
///
/// | 离环外缘 | 线性富余 | 相对峰值 |
/// |---------:|---------:|---------:|
/// |   0 px   |   0.399  |   1.00   |
/// |   8 px   |   0.115  |   0.29   |
/// |  20 px   |   0.053  |   0.13   |
/// |  43 px   |   0.017  |   0.04   |
///
/// 折算到本实现（环外半径 58px）：主瓣 τ≈7px 承担环口那圈「白环外面裹着的紧
/// 光」，残尾 τ≈40px 权重只有 0.06，负责把参考图那截拖到 40px 外的淡尾巴续上。
/// 上一版按 sRGB 数值直接读比值，把「255 附近的压缩」当成了真实亮度，于是反解
/// 出 τ=15/30 两根**过长**的指数 —— 结果环外糊成一团又宽又平的蓝雾，正是
/// 「底下有个大的发光光晕」的观感来源。指数必须在**线性域**里拟合。
///
/// 另外**不要把环口调亮**来「补存在感」：环的峰值亮度是环带自身的发射（已经
/// 打满 255），外晕再亮只会把环糊成一圈肥厚的白边。参考图里那圈白边只有 3~4px。
///
/// 这里**必须按像素**而不是按「图元半尺寸 × `BLOOM_SIGMA`」给形状。旧实现是
/// 后者：把环带（6% 宽）扔进一个 σ=0.4×半径 的高斯里，峰值只剩 0.06，再乘
/// `RING_GLOW_SCALE=0.10` 后等效 0.006 —— 实机上环干本就没有雾，只剩一圈硬
/// 白线，和参考图「白环外面裹着一团光」差得最远的一处就是这里。
///
/// 增益口径同圆盘：`bloom_linear = u_color * u_intensity * shape * u_bloom`，
/// 环的 `u_intensity` 是 5.99。注意 `u_bloom` 现在是 **0.125**（对齐上游
/// `resolveUnityBloomIntensity`，见 `render.rs` 的 `BLOOM_GAIN`），所以这里的
/// 增益是旧版（`u_bloom = 0.25`）的两倍口径，数值不能和旧版直接比。
/// 环的**外侧**走完整 6 级（σ 3…96）—— 这一支的形状是被参考站实测校验过的
/// 那一支（8 / 20 / 43px 三点分别 +14% / −8% / +14%），所以不动它，只把
/// 近似方式从「两根指数」换成真核。
const float RING_GLOW_SIGMA_MAX = 44.0;
const float RING_GLOW_GAIN = 0.007;
/// 环带外**中远场**那一圈柔光（σ 24~96，掐掉近场三级）。
///
/// 参考帧的环是「一条 4~7px 的细线 + 外面 30px 才收干净的柔光」。整条 mip 链
/// 按 1/σ 加权时 52% 的能量压在 σ=3 上，这条柔光根本长不出来：增益一调大，
/// 先变肥的是贴边那三四像素（细线读成白管），远场始终是暗的。
///
/// 拆成两支之后：近场由 `RING_GLOW_GAIN`（整链）负责，远场由这一支
/// （`bloom_mip_band(24, 96)`）负责，两个尺度各自标定。洞内那一侧按 0.5 倍给。
const float RING_WIDE_GAIN = 0.017;
/// 洞内柔光相对环外的比例。
///
/// **0.5 → 0.12**：参考帧 `17-27-14`（盘在、环在）的盘面从盘心到 0.85R 是平的
/// （107,167,255 → 124,179,255），洞内没有一圈 σ=24..96 的宽带雾。那圈宽带
/// 只该在**盘退场之后**（`17-27-11`）才看得见，而盘在的时候它照样按加法叠上
/// 去 —— 实测把盘心从 111 抬到 136、盘边抬到 177，整块盘读成「自己在发光的
/// 蓝球」，环带又和盘边焊死，细白线彻底看不出来。
const float RING_HOLE_WIDE_SCALE = 0.12;
/// 环**本体**发射的压缩系数（1.0 = 上游 `hdrIntensity 5.992157` 原样）。
///
/// 上游那份 HDR 是先过 Unity 后处理（曝光 + 色调映射）才上屏的；我们这里没有
/// HDR 帧缓冲，`gl_FragColor` 一写就被 8-bit 钳到 1.0。5.99 的强度乘上材质色
/// 之后三通道全部越界，环带里那 5px 一片纯白 —— 观感是「一根霓虹灯管」，而
/// 参考帧的环从来没白过。
///
/// 实测（盘边对齐、半径归一，`work/cmp_phase.py`）参考帧 `17-27-01` 的环带峰
/// 值是 (188,215,240)，环带两侧都有渐变；我们同一位置是 (236,255,255) 且向外
/// 三四个像素仍是 255。把环本体压到 0.26 之后显示值落到同一个量级，环带里
/// 重新出现「中间最亮、两侧渐隐」，白管感消失。
///
/// **只压本体，不压雾**：远场雾的能量口径是 `glow_source = u_color * u_intensity`，
/// 与这一支无关；参考帧环外的近场反而比我们亮（1.05R 处线性富余 3.52 对
/// 1.24），所以本体压暗省下来的「观感预算」要还给 `RING_EDGE_GAIN`。
const float RING_EMISSION_SCALE = 0.20;
/// 环**雾**的源色去饱和量（0 = 纯顶点蓝，1 = 纯白）。
///
/// 上游的 bloom 是对**已经过色调映射的整帧**做卷积，环核在那一帧里是白的
/// (254,255,255)，所以卷积出来的光晕也从白核往外晕。我们的 `glow_source`
/// 一直是 `u_color * u_intensity` —— 一道**纯蓝**的能量，于是环带本体就算
/// 打到 255 白，贴边那一圈雾也立刻把它重新染成青霓虹管，环看着「发光但
/// 不是白光」。
///
/// 0.55 让环带内外的近场雾跟着白核走（参考帧 1.0~1.15R 的色相是中性偏蓝），
/// 远场那截仍由 `u_color` 的蓝撑着，不会整圈发灰。
const float RING_GLOW_DESAT = 0.05;
/// 环带**内侧**那层补光（往洞里渗）。旧实现这里是 `0.0`：环内侧一律不出雾，
/// 于是环看起来只是一圈贴在圆盘上的白线，光全跑到外面去了。
///
/// 平台站 `17-27-11`（只有环、没有盘的那一帧）给出洞内的剖面：离环带内缘
/// 4px 处线性富余 0.135、16px 处 0.068、30px 处 0.030；同一帧环**外**侧对应
/// 位置是 0.242 / 0.152 / 0.113。也就是说洞内既暗一些（峰值 0.56 倍）也收得
/// 更急（τ≈17px 对 τ≈33px）。所以内支单独给一组 τ，不用外侧那组。
/// 环的辉光在**角向**上的软化半宽（Alpha 单位）。见使用处说明。
const float RING_GLOW_ANGULAR_SOFTNESS = 0.30;
/// **但 `17-27-11` 是「只有环」的帧，洞内是深色桌面。** `17-27-14`（满环 +
/// 圆盘）里洞内是一条**平的**饱和蓝面（0~0.8R 恒 158~179），环内支只在
/// 0.85R 之后才抬起来。把上面那组 τ 直接套到有盘的帧上，加法遍里雾从环内缘
/// 一路铺到圆心，叠在 160 的盘面上 → 0.5R 处 183、0.75R 处已经饱和 255。
/// 实测合成剖面（r/R 按环外半径归一）：
///
/// |    r/R   | 合成 sRGB | 其中来自环内支 | 参考站 17-27-14 |
/// |---------:|----------:|---------------:|----------------:|
/// |   0.25   |      167  |        +6      |          159    |
/// |   0.50   |      183  |       +23      |          161    |
/// |   0.75   |      255  |       +94      |          169    |
///
/// 于是整块圆盘都在发光，环带反而被洞内的强光淹没、轮廓看起来变薄 ——
/// 即「中间的蓝色圆一起发光、光环变薄」。口径收到只留贴环带的一条窄边
/// （τ 3.5 / 6px），洞内 17px 外的贡献降到 0.2/255 以下。
///
/// 下一轮若还嫌盘面亮，先把 `TRI_GLOW_NEAR_GAIN` 往下压 —— 洞内 0.5~0.72R
/// 剩下的那点抬升是碎片的雾，不是这里的残尾（此处 0.75R 已经没有贡献）。
///
/// 洞内支的 σ = **24px**（4 级），增益取外支的 **0.56 倍**。
///
/// 这两个数直接来自「只有环、没有盘」的那一帧 `17-27-11`：洞内离环带内缘
/// 4px 处线性富余 0.135、16px 处 0.068、30px 处 0.030；同一帧环**外**侧对应
/// 位置是 0.242 / 0.152 / 0.113。三点比值 0.56 / 0.45 / 0.27 —— 洞内既暗一些
/// 也收得更急，σ 比外支（96）小一档正好对上。
///
/// **不要再用「压小洞内支去换盘面干净」这条思路。** 盘现在排在环的雾**之上**
/// （`render.rs` 的 `draw_rank`），盘在的时候这支雾本来就被盘本体盖住了 ——
/// 盘面那条朝环边提亮的曲线由 `DISK_HOLE_LEAK_GAIN` 单独负责。把这支压到
/// 0.009 / σ12 的后果不是「盘面更干净」，而是**盘一消失洞里就全黑**：实机截图
/// 里环内沿内是一整块纯黑，外侧却有明显雾，就是这支被压没了（盘的生命周期比
/// 环短，后半段只剩环，洞内全靠它）。
const float RING_HOLE_SIGMA_MAX = 18.0;
/// 0.05332 → 0.018：**盘还在的时候**这支是直接加法叠在盘面上的。原值是按
/// 「只有环」的 `17-27-11` 标定的（那里洞内本来就该有 0.135 的富余），可是
/// 盘的生命期（0~200ms）里它会把整片盘面抬成一条上扬的坡：
/// 1600p t=120ms 实测 0.6R 处已是盘心的 1.40 倍、0.8R 处 2.79 倍，而参考帧
/// `17-27-01` 同一批位置是 1.06 / 1.17 —— 盘心到 0.85R 基本是平的。
/// 观感就是「蓝盘自己在发光」，环带外缘直接焊死在盘边，看不出细白线。
///
/// 环消失后洞里剩的雾仍然由这支提供，0.018 在盘退场（200ms 之后）只比原值
/// 暗 1/3，`17-27-11` 那种「洞里一片哑光」还在。
const float RING_HOLE_GAIN = 0.008;
/// （`RING_GLOW_END` 截断窗已删，理由同 `DISK_HALO_END`：σ=96 的高斯尾巴在
/// 86px 处还有 1.7%，窗在 86→108px 一刀压到 0，实机就是雾上那圈弧形暗边。
/// 四边形留白放大到 `RING_HALO_PX` 190px，残差 <1/255。）

/// 拖尾外晕采用近、中两段指数衰减（1080p 参考像素），从条带边缘起算。
///
/// 权重按实机参考截图的横向剖面重新标定。该图 685px 宽，是 1920 画面缩到
/// 0.357 倍，图上 1px ≈ 原始 2.8px。剖面在 x=250 处（笔带大致水平）取竖列，
/// 背景取列内远离笔画的稳定值（sRGB 44），核心峰值取 sRGB 206。
///
/// | 离轴距离（原始） | 相对核心的线性亮度 |
/// |-----------------:|-------------------:|
/// |          5.6 px  |              0.095 |
/// |         11.2 px  |              0.036 |
/// |         22.4 px  |              0.011 |
/// |         56   px  |             0.0010 |
///
/// **必须先扣背景再判定。** 上一版直接拿「相对列内最暗值」的比值（0.190 /
/// 0.118 / 0.057 / 0.019）当目标，那等于把「核心到背景的 162 级落差」按 255
/// 归一：在 112px 处参考其实已经回到背景，却被读成 0.019，于是反解出一根
/// τ≈44px 的指数，把雾做得又宽又亮。
///
/// 扣掉背景之后这条曲线比双指数还陡：5.6→22.4px 掉了 8.6 倍。这正是 mip 链
/// bloom 的真实形状 —— 最细的一级（mip 0，1~2px 的核）贡献贴着笔画的那圈亮边，
/// 再往外是几级不断降权的残尾。所以分成「近场亮边」+「远场低幅残尾」两段，
/// 而不是一根中等 τ 的指数。
///
/// 两段的分配按**逐像素竖切剖面**定（`正确的/Screenshot 2026-10-31 00-12-02`，
/// 1:1 裁切，扣掉该列远场背景、转到线性域后按 G 通道归一）：
///
/// | 离轴 | 参考线性 | 旧版 | 只用 0.8/τ9 | 本版 |
/// |-----:|---------:|-----:|------------:|-----:|
/// |  2px |    0.793 | 0.13 |       0.197 | 0.78 |
/// |  6px |    0.446 | 0.10 |       0.138 | 0.44 |
/// | 10px |    0.238 | 0.07 |       0.094 | 0.24 |
/// | 15px |    0.138 | 0.05 |       0.061 | 0.14 |
/// | 32px |    0.038 |  —   |       0.019 | 0.04 |
///
/// 两版都栽在同一处：**近场纯青的亮带外面没有那层饱满的光**，带子看上去只有
/// 两三像素宽（参考图里饱和核心就有 4px、200 以上一直到 6px）。把权重从
/// 0.36 抬到 0.80 只补回一半，因为衰减长度 9px 太长 —— 参考的斜率在 4~10px
/// 之间陡到 τ≈6，15px 之后才放缓到 τ≈20，80px 之外还有一根 τ≈60 的极淡残尾。
/// 单根 τ=9 无法同时对上「近处陡、远处缓」，所以拆成三根（0.06 那根就是长残尾）。
///
/// 注意权重可以大于 1：`glow_shape` 是乘到 `bloom_linear` 上的系数，不是遮罩。
/// 拖尾是**最标准**的线源：一条几像素宽的细带，所以 mip 链核的 1/σ 权重就是
/// 为它写的。开到 σ=48（5 级）：再往粗是第 6 级 σ=96，那一级会把整条笔画的
/// 雾摊到快 100px 外，实测 40px 处比现在亮 1.85 倍 —— 拖尾会糊成一条宽带，
/// 而参考图里雾是紧贴笔画的。
const float BLOOM_TRAIL_SIGMA_MAX = 48.0;
const float BLOOM_TRAIL_HALO_GAIN = 3.27150;
/// （`BLOOM_TRAIL_HALO_END` 截断窗已删，理由同 `DISK_HALO_END`：σ=48 的高斯
/// 尾巴在 96px 处还有 1.2%，窗在 96→120px 一刀压到 0 就是拖尾上方那道横切边。
/// σ=48 核在 165px 处残差降到 <0.3/255，`BLOOM_TRAIL_HALO_PX` 放大到 180px。）
/// 雾源卷积的抽头数（沿纹理 u 的中点采样，权重恒等归一）。
///
/// 5 个抽头在「整条笔画被抹成均值」的极限下已经足够（此时 5 点矩形平均与真实
/// 卷积的差远小于 8bit 量化）；而在贴笔画处窗口收缩到几个像素，5 个抽头也不比
/// 原来的三点帐篷更钝。**必须是编译期常量** —— GLES2 要求循环边界恒定。
///
/// 试过加到 9，想压掉慢速移动时那 1~3 级的亮度抖动：实测时域二阶差分没有变好
/// （±2 对 ±2），`r1` 标定场景也逐像素无差（max|d| = 1）。既然没有收益，就留在
/// 5 —— 多 4 次纹理解码换不来可见改善。
const int FOG_TAPS = 5;

/// 雾源卷积窗口的弧长半宽（1080p 参考像素）。
///
/// **必须与离笔画的远近无关。** 它只回答「雾源的均值该在多长一段笔画上取」，
/// 而「离得多远雾就多淡」完全由 `halo(edge_px)` 负责。两者相乘时，若窗口也随
/// 距离增长，均值上升会盖掉增益下降，靠近笔画端点处就会出现一圈比外侧更暗的
/// 暗沟（详见 `main()` 里 `fog_blur_px` 处的推导）。
///
/// 取 16px 是为了让**短笔画**自然退化成圆斑：`s6`（12px）与 `s7`（18px）在
/// 这个半宽下窗口已经盖满整条笔画，均值对最近点的位置不敏感；而 `r1`（600px+
/// 的长笔画）上窗口只覆盖局部几个百分点，雾色照旧跟着笔画走 —— 实测 `r1` 的
/// 纵向剖面在改动前后逐值只差 1 级。
const float BLOOM_TRAIL_FOG_BLUR_PX = 16.0;
/// 拖尾四边形沿切线前后各外扩的长度（1080p 参考像素）。
///
/// 相邻两段必须在几何上**重叠**：外扩区让两段的胶囊在关节处交叠，`GL_MAX`
/// 混合取到的两侧取值连续，转角不会漏光。段长本身只有 14~19px，扩出一个段长
/// 左右的余量足以盖住段端 AA、关节斜接以及渲染器取整带来的零点几像素误差。
/// shader 用 `u_extent.x - 2 × JOINT_PX` 反算真实段长，`render.rs` 的
/// `BLOOM_TRAIL_JOINT_PX` 必须与这里一致。
const float BLOOM_TRAIL_JOINT_PX = 22.0;
/// 笔画两端的光晕**不再**单独收口。
///
/// 早先这里有一个 `BLOOM_TRAIL_CAP_FADE`（45px 的圆形挖空）。它解决的问题
/// 「段末朝外半平面被整块掐掉」是旧实现拿 `along_raw` 当判据的副作用；换成
/// 距离场之后端点外本来就是平滑的球形衰减，再挖洞反而会把笔画中心挖成暗谷
/// （见 trail 分支末尾 `glow_shape` 的说明）。
/// 拖尾的 HDR 强度是 23.97（`trailArtisticIntensity`），这是**直射色**的增益；
/// 辉光不能直接套这个数。mip 链把一条 2~3px 宽的带子面积平均掉大半，真正漏进
/// 光晕的只剩一小部分。按官方图反解：取拖尾亮段的作者色 (0, 0.39, 1.0)，要让
/// 带子边缘处的线性蓝通道落在实测的 ≈1.3，系数就是 ≈0.25 —— 也就是说有效 HDR
/// 强度 23.97 × 0.25 ≈ 6，正好和圆盘/碎片用的 `UNITY_HDR_INTENSITY`(5.99) 同级。
///
/// 这个系数必须对整条拖尾**统一**：上游的雾是逐像素累加出来的能量，颜色暗的
/// 尾段本来就该比亮段暗。按最亮段标定、再用同一个系数乘下去，尾段的雾自然变
/// 弱；反过来按暗段标定，亮段的雾会糊成一整块过曝的方块。
///
/// 0.43 → 0.20 → 0.15 → 0.113 → 0.06 → 0.05：每次都跟着「雾源」口径与参考
/// 实测走。0.05 的标定：软膝版确定性渲染（见 `ce_scene` 的时间锚定）对 1:1
/// 参考的逐点实测，0.06 时头段 1.17 倍、中段 1.3~1.7 倍 —— 取 0.83 倍让头段
/// 归一（0.97），中段剩余 +20%~+40% 记作已知偏差（雾的纵向衰减形状比参考略
/// 平，单段 halo 近似无法同时压中段和保住尾段）。
const float BLOOM_TRAIL_SOURCE_SCALE = 0.05;
/// 光晕光源色向白色混一点 —— **拖尾不混**。
///
/// 官方图里拖尾的整条亮带 R 通道始终等于背景（实测核心 (28,255,255)，背景
/// (25,44,72)）：作者色 `evaluateUnityTrailColor` 的 R **恒为 0**，无论进度
/// 走到哪一段都没有红。按 4.5% 混白会凭空给整条带子加一截红（实测 R 从背景
/// 抬到 50），亮带就从「纯青」变成「发白的青」，和参考图差一整个色相。
///
/// 原先那 4.5% 是为了把雾的 G/B 比从 0.63 抬到 0.69 —— 但那个差值来自雾里
/// 混进的圆盘蓝，不是拖尾自己该白的部分。拖尾的雾必须保持纯青。
const float BLOOM_TRAIL_WHITE_MIX = 0.0;

/// 上游 `FxBloomSettings::threshold`（默认 1.0，gamma）。Unity 的 bright pass 在
/// **线性**域比较，`unityGammaToLinearChannel(1.0) == 1.0`，所以门槛就是线性 1.0。
const float BLOOM_THRESHOLD = 1.0;
/// 三角碎片的光晕（1080p 参考像素，从**三角形边缘**起算）。
///
/// 旧实现把碎片的雾做成「以图元中心为圆心、半径 0.45 半尺寸的高斯」—— 无论
/// 三角形朝哪边转，外面那圈光都是一个正圆，而且四边形留白是 0，光晕根本铺不
/// 出形状外。实机表现就是「碎片中心有一团白光」，而不是「整个三角形在发光」。
///
/// 现在改成对**三角形自己的 SDF** 取衰减：三个角、三条斜边各自向外扩散，形状
/// 跟着精灵一起转，和上游「对真实亮部做卷积」的观感一致。
///
/// 因为光晕要铺到三角形外侧，`render.rs` 的 `TRIANGLE_HALO_PX` 不能再是 0 ——
/// 它必须装得下 σ=24 核的自然尾巴（64px 处残差 <0.4/255），否则会被四边形切出
/// 一道直边。
/// 碎片开到 σ=24（4 级）。碎片很小，铺到 96px 会把整片点击区糊成一团；24px
/// 的雾正好裹住三角形又不越界。
const float TRI_GLOW_SIGMA_MAX = 24.0;
const float TRI_GLOW_GAIN = 0.045;

/// 碎片的直射亮度倍率。
///
/// 上游把碎片当 HDR 发光体（`hdrIntensity = 5.992`），但那份 HDR 是**先过
/// Unity 后处理的色调映射**才上屏的；我们这条链是直接预乘/加法进 8-bit 帧
/// 缓冲，5.99 一乘，三通道全部削平成纯白，三角形就成了一块白板。
///
/// 参考帧 `17-27-01` 里碎片填色是 (150,240,255) —— 蓝通道打满、红只有 150，
/// 也就是有效发射约 (0.30, 0.85, 1.0)。我们原样输出约 (1.0, 2.3, 3.3)，
/// 按 0.30 缩放后正好落回参考那条曲线，三角形的青蓝过渡也就回来了。
const float TRI_EMISSION_SCALE = 0.50;
/// （`TRI_GLOW_END` 截断窗已删：原来的 35→44px 淡出是在雾还有 ~3% 峰值时下刀，
/// 碎片外面那圈看得见的硬壳就是它。交给 σ=24 的核自然收尾即可。）

/// 图集格里那个三角形（局部归一化坐标，单位 = 图元半尺寸）。
///
/// 从 `ba_data/triangle_atlas.lz4` 的 alpha 实测：格内 128px 见方，底边横跨
/// x=3..125、高 105px —— 换算成 `local / (size/2)` 就是下面三个点。底边半宽
/// 0.953、高 1.633 ≈ 0.953·√3，是标准正三角形。
///
/// **两帧是上下镜像的**（`FX_TEX_Triangle_02_1` 是 2×1 图集，右格由左格垂直
/// 翻转得到）：左格尖朝下、右格尖朝上。这里统一按「尖朝上」描述，取两格实测
/// 的平均位置，左格在采样前把 `y` 取反即可。
const vec2 TRI_V_APEX = vec2(0.0, -0.9141);
const vec2 TRI_V_BASE_L = vec2(-0.9531, 0.7188);
const vec2 TRI_V_BASE_R = vec2(0.9531, 0.7188);

float srgb_to_linear_ch(float value) {
    float srgb = clamp(value, 0.0, 1.0);
    if (srgb <= 0.04045) {
        return srgb / 12.92;
    }
    return pow((srgb + 0.055) / 1.055, 2.4);
}

vec3 srgb_to_linear(vec3 color) {
    return vec3(
        srgb_to_linear_ch(color.r),
        srgb_to_linear_ch(color.g),
        srgb_to_linear_ch(color.b));
}

float linear_to_srgb_ch(float value) {
    float linear = clamp(value, 0.0, 1.0);
    if (linear <= 0.0031308) {
        return linear * 12.92;
    }
    return 1.055 * pow(linear, 1.0 / 2.4) - 0.055;
}

vec3 linear_to_srgb(vec3 color) {
    return vec3(
        linear_to_srgb_ch(color.r),
        linear_to_srgb_ch(color.g),
        linear_to_srgb_ch(color.b));
}

/// 三角形的有符号距离场（局部归一化坐标，单位 = 图元半尺寸）。
///
/// 取三条边的最短距离，用第三个分量判内外 —— 对凸多边形是精确解，比「外接圆
/// 距离」贴合真实轮廓得多。没有循环，GLES2 上完全安全。
float sd_triangle(vec2 p, vec2 v0, vec2 v1, vec2 v2) {
    vec2 e0 = v1 - v0;
    vec2 e1 = v2 - v1;
    vec2 e2 = v0 - v2;
    vec2 w0 = p - v0;
    vec2 w1 = p - v1;
    vec2 w2 = p - v2;
    vec2 pq0 = w0 - clamp(dot(w0, e0) / dot(e0, e0), 0.0, 1.0) * e0;
    vec2 pq1 = w1 - clamp(dot(w1, e1) / dot(e1, e1), 0.0, 1.0) * e1;
    vec2 pq2 = w2 - clamp(dot(w2, e2) / dot(e2, e2), 0.0, 1.0) * e2;
    float side = sign(e0.x * e2.y - e0.y * e2.x);
    vec2 d = min(
        min(vec2(dot(pq0, pq0), side * (w0.x * e0.y - w0.y * e0.x)),
            vec2(dot(pq1, pq1), side * (w1.x * e1.y - w1.y * e1.x))),
        vec2(dot(pq2, pq2), side * (w2.x * e2.y - w2.y * e2.x)));
    return -sqrt(d.x) * sign(d.y);
}

/// 像素单位的 SDF → 覆盖度：`d < 0` 落在形状内，边界处一个像素宽的 smoothstep
/// 过渡给出亚像素级渐变，斜向边界不再有阶梯。
float aa_coverage(float signed_distance) {
    return 1.0 - smoothstep(-AA_HALF_WIDTH, AA_HALF_WIDTH, signed_distance);
}

/// ── 辉光的形状：上游 mip 链 bloom 的核 ──────────────────────────────────
///
/// 之前每一类元素都用「几根不同 τ 的指数相加」近似雾的形状，再在末尾乘一个
/// `1 - smoothstep(0.8·E, E, d)` 把尾巴切掉。那套写法有两个**结构性**缺陷，
/// 靠调参数是调不掉的：
///
///   1. `exp(-d/τ)` 自带一个特征尺度 τ。几根指数相加时，短的那根死掉、长的
///      那根接手的地方会鼓出一个可见的「肩」—— 半径扫过去先是陡、再是平、
///      最后又陡，读起来就是一段一段的。
///   2. 末尾那个截断窗把还在 3~8/255 上的尾巴直接压到 0（圆盘在 32→40px、
///      碎片在 11→14px 各有一刀），这就是「消失时的割裂感」。
///
/// 上游不是这么做的。`src/webgl2-bloom.js` 的真实流程是
/// PREFILTER → 6 级降采样（2×2 box，×0.25）→ 6 级升采样 → FINAL 4-tap box
/// × `u_intensity`，最后以 `One/One` 加回 Scene。关键在升采样那一行
///
///     outColor = accumulatedCoarse + currentFine;
///
/// **权重恒为 1、不归一**。也就是说每一级贡献的是「同样多的一份能量」，只是
/// 被摊在不同的 σ 上：对线源（细长的环带、笔画、盘沿）第 k 级的幅值
/// ∝ 1/σ_k，σ_k = σ0 · 2^k。
///
/// 两个常数都取自源码，不是拟合出来的：
///   · `resolvePyramid()`：`resolutionScale = 0.5`、`diffusion = 7`
///     （`src/config.js`）→ maxSize = 960、logIterations = log2(960) + 7 − 10
///     = 6.907 → **levelCount = 6**。
///   · σ0：level 0 是半分辨率（1 texel = 2 显示像素），再叠上降/升采样各级
///     box 的累积，等效 σ0 ≈ 3 显示像素。
///
/// 校验（参考站 `17-27` 系列环外剖面，扣背景转线性域后归一）：
///
/// |    d   | 参考 | σ0=3 / 6 级 | 差 |
/// |-------:|-----:|------------:|---:|
/// |  8 px  |0.288 |    0.328    |+14%|
/// | 20 px  |0.133 |    0.122    | −8%|
/// | 43 px  |0.043 |    0.049    |+14%|
///
/// 远场 d >> σ0 时这个和收敛到 **∝ 1/d 的幂律**：没有特征尺度，所以参考图里
/// 雾是「亮 → 暗 → 没了」一条连续曲线，既没有肩也没有切断点 —— 最粗那一级的
/// 高斯自己就把尾巴收干净了，不需要额外乘窗。
const float BLOOM_MIP_SIGMA0 = 3.0;
const int BLOOM_MIP_LEVELS = 6;

/// `sigma_max`：参与累加的最粗一级，决定雾**自然**消失的距离。
///
/// 各类元素取不同的值，理由见各自的常量。它不是「截断」，而是把金字塔截在不
/// 同的高度 —— 上游整条链也只有 6 级，本来就是有限的。
float bloom_mip(float d, float sigma_max) {
    float acc = 0.0;
    float norm = 0.0;
    float sigma = BLOOM_MIP_SIGMA0;
    for (int level = 0; level < BLOOM_MIP_LEVELS; level++) {
        // 超过 sigma_max 的级直接不参与（权重 0），归一化也随之改变，
        // 所以 kernel(0) 恒为 1 —— 增益的含义统一成「贴着形状那一点的雾强」。
        float amp = step(sigma, sigma_max + 0.001) / sigma;
        float x = d / sigma;
        acc += amp * exp(-0.5 * x * x);
        norm += amp;
        sigma *= 2.0;
    }
    return acc / max(norm, 1.0e-6);
}

/// 只取**中远场**那几级（σ ∈ [`sigma_min`, `sigma_max`]）的 mip 核。
///
/// 整条链按 1/σ 加权时，σ=3 那一级占了归一化后的 **52%** —— 对「薄发光带」
/// 这类源来说，这个分配把能量全堆在贴边的三四像素里，远场只剩尾巴的尾巴。
/// 参考帧的环不是这样：带子本身只有 4~7px，外面却拖着一片 30px 才收干净的
/// 柔光。按整条链给增益只能二选一（要么核肥、要么远场没了），所以远场单独
/// 用这条**掐掉近场**的核来补，两个半径尺度就可以各调各的。
///
/// `kernel(0)` 仍恒为 1，增益语义与 `bloom_mip` 一致。
float bloom_mip_band(float d, float sigma_min, float sigma_max) {
    float acc = 0.0;
    float norm = 0.0;
    float sigma = BLOOM_MIP_SIGMA0;
    for (int level = 0; level < BLOOM_MIP_LEVELS; level++) {
        float amp = step(sigma, sigma_max + 0.001)
            * step(sigma_min - 0.001, sigma)
            / sigma;
        float x = d / sigma;
        acc += amp * exp(-0.5 * x * x);
        norm += amp;
        sigma *= 2.0;
    }
    return acc / max(norm, 1.0e-6);
}

/// 点到线段的距离。
///
/// 每个四边形只算**本段**；整条折线的「取最近」由离屏缓冲的 `GL_MAX` 混合
/// 完成（`max_i exp(-d_i/σ) ≡ exp(-min_i d_i/σ)`），见 `render.rs` 的
/// `TrailTarget`。
float trail_seg_distance(vec2 p, vec2 a, vec2 b) {
    vec2 ab = b - a;
    float length_sq = dot(ab, ab);
    if (length_sq < 1.0e-6) {
        return length(p - a);
    }
    float t = clamp(dot(p - a, ab) / length_sq, 0.0, 1.0);
    return length(p - (a + ab * t));
}

/// 精灵局部坐标（单位 = 图元半尺寸，已反旋）。形状 SDF 都在这个坐标系里算。
vec2 sprite_local(vec2 pixel) {
    vec2 delta = pixel - u_center;
    float cosine = cos(u_rot);
    float sine = sin(u_rot);
    vec2 local = vec2(
        cosine * delta.x + sine * delta.y,
        -sine * delta.x + cosine * delta.y);
    return local / max(u_size * 0.5, 0.0001);
}

/// 精灵（圆盘 / 三角）的 uv：反旋到精灵局部，归一化后套上图集窗口。
vec2 sprite_uv(vec2 pixel) {
    vec2 normalized = sprite_local(pixel);
    return vec2(
        u_uv_off.x + (normalized.x * 0.5 + 0.5) * u_uv_off.y,
        normalized.y * 0.5 + 0.5);
}

/// 溶解环的 uv：按 `makeUnityRingMesh()` 的展开方式从极坐标重建。
/// 环带的实际内径（含视觉加厚）。几何与纹理必须用**同一个**值，否则采样会
/// 沿着环带偏出去，环内沿出现一圈半透明的纹理边缘。
float ring_inner_radius(float outer) {
    return outer * max(u_ring_inner - RING_THICKEN, 0.0);
}

vec2 ring_uv(vec2 pixel, float radius) {
    float outer = u_size * 0.5;
    float inner = ring_inner_radius(outer);
    float radial = (radius - inner) / max(outer - inner, 0.0001);
    // 网格顶点是 `center + R(rot) * mesh`，且网格 y 轴在屏幕上翻转过，
    // 所以局部角 = atan2(-dy, dx) - rot。
    float angle = atan(-(pixel.y - u_center.y), pixel.x - u_center.x) - u_rot;
    float progress = fract((angle - RING_FIRST_ANGLE) / (2.0 * PI));
    return vec2(
        RING_UV_INSET + RING_UV_SPAN - RING_UV_SPAN * progress,
        RING_UV_INSET + RING_UV_SPAN * radial);
}

void main() {
    vec2 pixel = (input_to_geo * vec3(niri_v_coords, 1.0)).xy;
    float global_opacity = clamp(u_gop, 0.0, 1.0);
    // 物理像素 → 1080p 参考像素。辉光是屏幕空间量，各分支都要用。
    float ref_scale = max(u_ref_scale, 0.0001);
    // 辉光半径倍率：把「距离 / 半径」统一缩放，等于整体放大或缩小所有的雾。
    float glow_radius = max(u_glow_radius, 0.0001);

    vec3 emission = vec3(0.0);
    float coverage = 0.0;
    // 辉光的「源色」：图元在被形状完整覆盖处的线性发光色。
    vec3 glow_source = vec3(0.0);
    // 辉光形状因子：1 在形状里、向外平滑衰减。
    float glow_shape = 0.0;
    // 原料自带的不透明度（顶点色 alpha）。辉光必须跟着它一起淡出，否则图元快
    // 消失时实体的覆盖度已经很小、外圈光晕却还是满的，中心会塌成一个黑洞。
    float element_opacity = clamp(u_color_a, 0.0, 1.0);

    if (u_mode < 0.5) {
        // CrossPixel：R 通道当形状遮罩，覆盖度 = shape * color.a。
        vec4 texel = texture2D(tex_disk, sprite_uv(pixel));
        float half_px = max(u_size * 0.5, 0.0001);
        float disk_shape_px = DISK_SHAPE_RADIUS * half_px;
        float shape = srgb_to_linear_ch(texel.r);
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * shape
            * global_opacity;
        coverage = clamp(shape * u_color_a, 0.0, 1.0) * global_opacity;

        // ── 盘面自发光（见 `DISK_RIM_*`）──────────────────────────────────
        //
        // 「蓝盘在发光」必须写进 `emission`，不能靠雾：雾是 rank 0 的加法遍，
        // 盘本体（不透明预乘 over，rank 2）会把它在盘内整片扣掉 —— 盘面上什么
        // 都不剩，那正是之前「蓝盘没有发光」的根因。
        //
        // 做法是把颜色朝**自己的峰值通道**推：盘面 B 早就打在 255，再往线性域
        // 加能量只会抬高 `peak`，而收尾的 `capacity = alpha / peak` 会按新峰值
        // 把整块盘等比压回去 —— 实测加 0.19 线性只换来 +1 级，甚至更暗。所以
        // 这里乘增益后**夹回原峰值**：B 纹丝不动，R/G 追上来，盘边读作「亮到
        // 发白」，而峰值不变、容量压缩因此是恒等变换。
        float disk_rr = length(pixel - u_center) / disk_shape_px;
        float disk_rim = smoothstep(DISK_RIM_START, DISK_RIM_END, disk_rr);
        float disk_peak = max(emission.r, max(emission.g, emission.b));
        emission = min(
            emission * mix(1.0, DISK_RIM_GAIN, disk_rim),
            vec3(disk_peak));
        // 辉光的光源色用**编码域**的蓝，不是线性域的蓝。
        //
        // 上游 bloom 的 mip 链是在已经做过 `LinearToSrgb` 的帧上做的卷积。官方图
        // 盘外远场反解出来的线性通道比例是 (0.24, 0.39, 1.0) —— 正好是作者蓝
        // (0.2406, 0.3906, 1.0) 的 sRGB 值，而不是它的线性值 (0.048, 0.127, 1.0)。
        // 拿线性色当光源，外晕会缺红缺绿、偏成青色 —— 这就是「圆盘颜色不对、
        // 整体发青」的来源。
        vec3 disk_azure = linear_to_srgb(u_color);
        // 到盘边的距离（1080p 参考像素，盘内为负）。
        float disk_edge_px =
            (length(pixel - u_center) - DISK_SHAPE_RADIUS * half_px) / (ref_scale * glow_radius);
        float disk_out = max(disk_edge_px, 0.0);
        glow_source = mix(disk_azure, vec3(1.0), DISK_HALO_WHITE_MIX) * u_intensity;
        // 盘外那半支。`disk_out` 在盘内恒为 0、`bloom_mip(0)` 恒为 1，所以必须
        // 用贴边门控把盘内压掉 —— 否则整片盘面平白亮起一档（旧版是靠
        // `DISK_HALO_INSIDE_SCALE = 0` 做的同一件事，这里直接写进门控里）。
        float disk_halo = (DISK_HALO_GAIN
            * bloom_mip(disk_out, DISK_HALO_SIGMA_MAX)
            + DISK_EDGE_GAIN * bloom_mip(disk_out, DISK_EDGE_SIGMA))
            * smoothstep(-5.0, 0.0, disk_edge_px);
        // 盘内那半支**不在这里做**：这一遍（`u_pass == 1`）排在 rank 0，盘本体
        // 会把它在盘内整片扣掉。盘面朝边渐亮由本体自己的发射项给出，见上方
        // `DISK_RIM_*`。这里只留盘外那圈真正漏出去的雾。
        // 环的内层雾被盘本体挡掉了，按盘的几何补回来（幅度见
        // `DISK_HOLE_LEAK_GAIN`）。盘心离环内沿最远，补得最少；越靠盘边越多
        // —— 正好复现参考帧那条朝环边提亮的曲线。
        //
        // 形状是「**盘心 0.75R 内完全不加光，最后 25% 才抬起来**」，不是从圆心
        // 开始的线性斜坡。两条独立测量都指向同一个形状：
        //   · 参考帧 `17-27-14` 盘内 R = 108/108/109/111/114/120（0→0.8R）；
        //   · 实机 1600p 截图 `01-06-55` 横向剖面 r=0→70 恒在 157±1，只有贴
        //     环边才抬到 169。
        // 旧写法 `(r/Ri) * smoothstep(0, 0.12, r/Ri)` 是从圆心起坡，把整个盘面
        // 中段抬了一档（实测 0.3R 处 +21、0.7R 处 +30，盘心反而因为收口留出一个
        // 暗核），读起来是「会发光的蓝球」+「中间一个灰点」。改成末段抬升之后
        // 盘面回到均匀蔚蓝，亮起来的地方正好是参考里贴环的那一圈。
        //
        // 盘的形状半径（像素）。**leak 必须在盘外归零**：这个四边形的留白是
        // `DISK_HALO_PX` 190px，而 `t_leak` 在盘外恒为 0 —— 不乘这个门控的话
        // leak 会取满值铺满整个四边形，点击就是一块 400px 见方的均匀亮块。
        float ring_inner_px = DISK_RING_INNER_RATIO * disk_shape_px;
        float t_leak = clamp(
            max(ring_inner_px - length(pixel - u_center), 0.0) / ring_inner_px,
            0.0,
            1.0);
        float k_leak = smoothstep(0.75, 1.0, 1.0 - t_leak);
        float inside_disk = 1.0
            - smoothstep(disk_shape_px - 3.0, disk_shape_px + 3.0,
                         length(pixel - u_center));
        glow_shape = disk_halo
            + DISK_HOLE_LEAK_GAIN * k_leak * inside_disk;
    } else if (u_mode < 1.5) {
        // DissolvePixel：只保留 alpha 高于阈值的部分。
        float radius = length(pixel - u_center);
        float outer = u_size * 0.5;
        float inner = ring_inner_radius(outer);
        // 环带的内外两条边是纯几何边界，硬比较会在斜向处出现阶梯，改用 SDF
        // 的 smoothstep 羽化。采样照旧放在统一控制流里，避免 GLES2 对非一致
        // 分支内取纹理的未定义行为。
        float ring_coverage = aa_coverage(max(inner - radius, radius - outer));
        vec4 texel = texture2D(tex_ring, ring_uv(pixel, radius));
        // 溶解阈值保持上游的硬裁语义，但在阈值两侧留一条极窄的过渡带（Alpha
        // 单位），溶解前沿才不会有格子状跳变。
        float material_alpha = texel.a * u_color_a;
        float dissolve = smoothstep(
            u_threshold - DISSOLVE_SOFTNESS,
            u_threshold + DISSOLVE_SOFTNESS,
            material_alpha);
        // 覆盖度**必须**继续乘纹理 alpha。上游 `DissolvePixel` 的原文是：
        //
        //     clip(materialCoverage - threshold);
        //     coverage = materialCoverage * globalOpacity;
        //
        // `clip` 只决定「画不画」，画出来的像素仍按 `materialCoverage` 取不
        // 透明度。断口正好落在 `materialAlpha == threshold` 上，那里剩下的不
        // 透明度就等于阈值本身（0~1 的中间值），所以擦出前沿是**渐变收尾**。
        //
        // 曾经为了「环不够亮」把这里改成按顶点色实心画（`u_color_a * dissolve`），
        // 亮度是对了，但断口处的不透明度变成 1.0、往缺口方向一步跳到 0 —— 实机
        // 「长按只移动一点距离」时，环像被刀切掉一大段，就是这条跳变。真正的
        // 亮环问题在别处（见下面 `RING_GLOW_SCALE` 的说明），不该靠这里补。
        // 径向软边：环带的内外两条边不是硬边，而是一段 2~3px 的渐变。
        float ring_edge_distance = min(radius - inner, outer - radius);
        float ring_soft = smoothstep(
            0.0, RING_EDGE_SOFTNESS * ref_scale, ring_edge_distance);
        float ring_opacity = material_alpha * dissolve;
        float material_coverage = ring_opacity * ring_coverage * ring_soft;
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * RING_EMISSION_SCALE
            * material_coverage
            * global_opacity;
        // 高光去饱和：重演上游 HDR → Unity 色调映射那一步。
        //
        // `ring_color` 把顶点色从白渐变成 (0.297,0.653,1.0)，乘上 `hdrIntensity`
        // 之后蓝通道仍是 5.99、红通道只有 1.78 —— 这在 HDR 里是「很蓝的亮」，
        // 但 Unity 上屏前要过色调映射，而任何 filmic 曲线在接近白点时都会
        // **降低饱和度**（ACES 尤其明显）。结果是参考帧的环核从头到尾都是
        // (255,255,255) / (215,228,242) 这种近白色，从来不是青色霓虹管。
        //
        // 我们没有 HDR 帧缓冲（一写就钳到 1.0），钳位是**保色相**的，于是同样
        // 的能量出来是 (145,207,250) —— 实机观感「后期那道弧发青、发灰」就是
        // 这一步缺的。这里按自身峰值把接近白点的像素拉向等亮度灰（= 去饱和），
        // 环带外沿 alpha 低、峰值也低，仍然保留蓝色。
        float ring_peak = max(emission.r, max(emission.g, emission.b));
        float ring_desat = smoothstep(0.38, 0.82, ring_peak);
        emission = mix(emission, vec3(ring_peak), ring_desat);
        coverage = clamp(material_coverage * global_opacity, 0.0, 1.0);

        // 辉光的可见度只跟「这道环整体还剩多少」有关 —— 而且必须按**环带中线**
        // 取，不能用当前像素的 alpha。
        //
        // 这是「环完全不发光」的根因：环带之外的纹理 alpha 是 0，若拿当前像素的
        // `ring_opacity` 当辉光强度，`glow_shape` 算得再对也会被乘成 0 —— 实测
        // 环外一整片纯背景 (20,20,20)，一点光都铺不出去。圆盘和碎片没这个问题，
        // 它们的 `element_opacity` 取的是顶点 alpha（常数），与像素位置无关。
        //
        // 按径向中线采样，得到的就是「这个角度上环还剩多少」：辉光因此跟着溶解
        // 前沿的角度变化走，而不会在环带内外断掉。
        vec2 ring_mid_uv = ring_uv(pixel, mix(inner, outer, 0.5));
        float ring_mid_alpha = texture2D(tex_ring, ring_mid_uv).a * u_color_a;
        // 角向过渡必须比实体那条**宽得多**。
        //
        // 实体用的是 `DISSOLVE_SOFTNESS`（0.02）—— 那是「边界上一个像素」的量级，
        // 对实体正合适。但辉光是整条弧的能量摊开来的，缺口处应该是一段几十度
        // 的渐变；照搬 0.02 会让辉光沿缺口边缘被一刀切开，从圆心射出一道硬邦邦
        // 的黑色楔形（实测就是这样）。
        element_opacity = smoothstep(
            u_threshold - RING_GLOW_ANGULAR_SOFTNESS,
            u_threshold,
            ring_mid_alpha);

        // 环的雾按**到环带**的距离给，不是按「到外缘的距离」。
        //
        // 环带是有厚度的闭合形状，它的辉光应该**贴着这条带子**：带子上最亮，
        // 向外扩散，向内也往洞里渗。旧实现只朝外衰减、内侧一律掐成 0，加上上面
        // 那个 per-pixel alpha 的问题，环在实机上干脆一点光都没有，只剩一圈贴在
        // 圆盘上的白线。
        //
        // **内、外两侧必须是各自独立的一支**，不能合成 `max(inner-r, r-outer)`
        // 再共用一组增益 —— 那个写法里 `ring_out` 在环带内侧也是 0，于是外晕的
        // 整组指数会原封不动地铺满整个洞，环里亮得像块实心圆盘；再叠上补光项，
        // 外侧还会多拖出一条根本不该存在的长尾（r=100 处实测 68，参考只有 40）。
        // 拆开之后外侧只剩主瓣 + 残尾，内侧单独一支、按 0.62 倍并且更短的 τ 给
        // —— 参考图里洞内确实比环外暗、且收得更急。
        // 到**环带**的距离：带子里面恒为 0，带外按到最近那条边的距离算。
        //
        // 旧写法让内、外两支各自量「到内沿」「到外沿」的距离 —— 于是两支的核
        // 中心分别落在带子的两条边上，模糊出来的是一条**双峰**：r=68 和 r=74
        // 各有一圈亮，中间 r=70~71 凹下去 30 级，实机看就是「环里还套着一条
        // 更暗的环」。上游的 bloom 是对整张发光图做卷积，一条有厚度的带子卷完
        // 只能得到**平顶 + 两侧单调衰减**，绝不会在带宽内部凹下去。
        //
        // 所以两支共用同一个「到带的距离」，只在带子外面分岔（内侧用更短的 τ，
        // 参考里洞内的光确实比环外收得急）。
        float d_band = max(max(inner - radius, radius - outer), 0.0)
            / (ref_scale * glow_radius);
        float d_out = d_band;
        float d_in = d_band;
        // 内、外两支**平滑**混合，不再拿 `step` 在环带中线上硬切。
        //
        // 旧写法 `step(中线, radius)` 跨过中线时幅值从 0.12 跳到 1.0、衰减长度
        // 从 3.5 跳到 7 —— 环带上凭空多出来一道接缝，半径扫过去会看到一圈硬
        // 边。改成按半径在 `[inner, outer]` 上 smoothstep，两支各自求值后再插
        // 值，整个径向域 C1 连续（环带内两支都取 1，插值只改变权重，不会在
        // 距离上做混合，所以不会出现「把两个距离搅在一起」的怪形状）。
        float w_out = smoothstep(inner, outer, radius);
        glow_source = mix(u_color, vec3(1.0), RING_GLOW_DESAT) * u_intensity;
        float ring_halo = mix(
            RING_HOLE_GAIN * bloom_mip(d_in, RING_HOLE_SIGMA_MAX)
                + RING_WIDE_GAIN * RING_HOLE_WIDE_SCALE
                    * bloom_mip_band(d_in, 48.0, 96.0)
                + RING_EDGE_GAIN * bloom_mip(d_in, RING_EDGE_SIGMA),
            RING_GLOW_GAIN * bloom_mip(d_out, RING_GLOW_SIGMA_MAX)
                + RING_WIDE_GAIN * bloom_mip_band(d_out, 48.0, 96.0)
                + RING_EDGE_GAIN * bloom_mip(d_out, RING_EDGE_SIGMA),
            w_out);
        // 雾必须**跟着溶解一起退**，不能只在最后被门控一刀关掉。
        //
        // 上面的 `element_opacity` 是 `smoothstep(threshold - 软宽, threshold, 中线alpha)`：
        // 阈值离中线 alpha 还远时它恒为 1，跨过去就在 0.30 个阈值单位内掉到 0。
        // 于是整个后半生雾一直满值（实测 t=0.40~0.60 总能量 5898→5349，只降 9%），
        // 末帧图元被移除时整片雾**同时消失** —— 这就是「光晕消失太突兀」。
        //
        // 上游的 bloom 是每帧对**当前帧的发光像素**做卷积，弧溶解掉多少，雾的能量
        // 就少多少。这里用 `1 - threshold` 当「还剩多少」的代理：阈值曲线在
        // 0.2→1.0 段单调升到 1，正好对上剩余面积。前半段（阈值 < 0.30）不参与，
        // 免得把中期的雾压暗；0.30→0.90 之间平滑过渡到按比例衰减，末端自然归零。
        float ring_remaining = 1.0 - clamp(u_threshold, 0.0, 1.0);
        element_opacity *= mix(
            1.0,
            ring_remaining,
            smoothstep(0.30, 0.90, u_threshold));
        glow_shape = ring_halo;
    } else if (u_mode < 2.5) {
        // AdditivePixel：纯发光，不做溶解裁剪。
        // 图集左右两帧各占半张纹理，四边形又比精灵大一圈，所以采样既要夹在本帧
        // 格子里（否则会采到隔壁那半张的三角形，也会在图集接缝上做双线性混色），
        // 又要对越界部分按透明处理。
        vec2 uv = sprite_uv(pixel);
        vec2 cell_min = vec2(u_uv_off.x, 0.0);
        vec2 cell_max = vec2(u_uv_off.x + u_uv_off.y, 1.0);
        vec4 texel = texture2D(tex_triangle, clamp(uv, cell_min, cell_max));
        texel *= step(cell_min.x, uv.x)
            * step(uv.x, cell_max.x)
            * step(cell_min.y, uv.y)
            * step(uv.y, cell_max.y);
        float material_coverage = texel.a * u_color_a;
        emission = srgb_to_linear(texel.rgb)
            * u_color
            * u_intensity
            * TRI_EMISSION_SCALE
            * material_coverage
            * global_opacity;
        coverage = material_coverage * global_opacity;

        // 辉光跟着**三角形**走，不是跟着外接圆走。
        //
        // 图集左半格是右半格的垂直镜像（尖朝下），采样形状前先翻回来 —— 不翻
        // 的话光晕会一帧对得上、一帧整个倒过来。
        vec2 tri_local = sprite_local(pixel);
        float tri_flip = u_uv_off.x < 0.25 ? -1.0 : 1.0;
        vec2 tri_p = vec2(tri_local.x, tri_local.y * tri_flip);
        float tri_half_px = max(u_size * 0.5, 0.0001);
        float tri_edge_px = sd_triangle(tri_p, TRI_V_APEX, TRI_V_BASE_L, TRI_V_BASE_R)
            * tri_half_px / (ref_scale * glow_radius);
        float tri_out = max(tri_edge_px, 0.0);
        glow_source = u_color * u_intensity * TRI_EMISSION_SCALE;
        float tri_halo = TRI_GLOW_GAIN
            * bloom_mip(tri_out, TRI_GLOW_SIGMA_MAX);
        glow_shape = tri_halo;
    } else {
        // TrailPixel：发光不被覆盖度压暗，覆盖度只管 Alpha 与纵向淡出。
        vec2 relative = pixel - u_origin;
        // 沿切线的**像素**投影（段起点为 0）。四边形在纵向前后各扩展了
        // `BLOOM_TRAIL_JOINT_PX`，所以它的取值域是 [-E, 段长 + E]。
        float along_raw = dot(relative, u_axis.xy);
        float across = dot(relative, u_axis.zw) / max(u_extent.y, 0.0001) + 0.5;
        // 段内归一化坐标，供颜色渐变 / 纹理 / 覆盖度使用。
        //
        // **必须按真实段长归一化，不能用 `u_extent.x`**。`u_extent.x` 是「段长 +
        // 两端各一个扩展区」，比段长多出 2 × `BLOOM_TRAIL_JOINT_PX`。拿它归一化会
        // 同时犯两个错：
        //   1. 亮带被画到段尾之外 22px（沿方向越界到邻居的地盘），相邻两段的带子
        //      在关节处重叠 —— 而这里是预乘 over 合成，不是相加，后画的把先画的
        //      整块擦掉。四边形边界上实测残留亮度 254/255，就是拖尾上那些带直边
        //      的矩形块。
        //   2. `u_progress` / `u_cov` / 颜色渐变只走到 length / (length + 44)
        //      ≈ 0.62，拖尾的纵向淡出与配色被整体拉长 1.6 倍。
        float joint_px_len = BLOOM_TRAIL_JOINT_PX * ref_scale;
        float seg_len = max(u_extent.x - 2.0 * joint_px_len, 1.0);
        float along = along_raw / seg_len;
        float particle_opacity = clamp(u_color_a, 0.0, 1.0);
        vec3 segment_color = mix(u_color, u_color_b, clamp(along, 0.0, 1.0));
        // 纵向覆盖必须算在分支外面：辉光是在分支外求的，如果只在采样分支里算，
        // 已经淡出到 0 的尾段（插值色是黑的）就会在四边形留白处被光晕画成一道
        // 黑边。
        float longitudinal = mix(u_cov.x, u_cov.y, clamp(along, 0.0, 1.0));
        // `element_opacity` **只跟顶点 alpha 走**，不乘 `longitudinal`。
        //
        // `u_cov` 是沿进度的**实体**覆盖度，上游曲线在 progress ≤ 0.2485 处恒为
        // 0 —— 也就是说拖尾总有四分之一长的一段本来就没有实体线。早期把它乘进
        // `element_opacity`，而 `element_opacity` 又是 `bloom_weight` 的因子，于是
        // 那一段的雾跟着一起归零：实测就是「一刀切，直接光晕就没了」。上游 bloom
        // 是整帧卷积，实体有没有不影响雾铺不铺。
        element_opacity = particle_opacity;

        // ── 距离场：只用**本段**，整条笔画的并集交给外层混合 ──────────────
        //
        // 这里曾经是「本段 + 前后各两段」的五段最小距离，再让「到本段最近」
        // 的一段独占该像素（`owns`）。那套机制是一堆几何硬伤的来源：
        //   · 光晕半径 120px 远大于段长（弯道上每段约 45px），±2 段的窗口根本
        //     盖不住「隔壁第三段」的贡献 —— 快速拖动折返时，两臂之间那片的
        //     距离场被高估，雾里留下一道楔形缺口；
        //   · `owns` 只能和紧邻两段做双向比较，非相邻四边形的光晕重叠时谁都不
        //     让位，预乘 over 叠成 `2a - a²`，转角处就是一块硬边亮斑。
        //
        // 病根是「用一个四边形的 draw 去近似整条折线的距离场」。正确做法是让
        // **每个四边形只负责自己那一段**，把「取最近」交给混合方程：各段以
        // `GL_MAX` 混合进离屏缓冲，得到的正是 `max_i exp(-d_i / σ)` =
        // `exp(-min_i d_i / σ)`，即整条折线的距离场 —— 数学上恒等，且
        // max 对「段集合」满足并集律，重叠不叠亮、缺口自动补齐
        // （见 `render.rs` 的 `TrailTarget`）。
        //
        // 因此这里只需本段到像素的距离。段末端由 Rust 侧直接给定，不做反算。
        float d_self = trail_seg_distance(pixel, u_origin, u_seg_end);
        float half_width = u_extent.y * 0.5;
        float body = aa_coverage(d_self - half_width);

        // 纹理采样提到 body 判断之外：雾的源色也用这份 texel（见下方
        // `pixel_emission`）。uv_along 在段外取 0/1，关节两侧取到相邻段同一
        // 进度，颜色连续。
        float uv_along = clamp(along, 0.0, 1.0);
        // u 必须夹在首/尾纹素中心之间 —— 纹理默认是 `GL_REPEAT`，而 `u_progress`
        // 的两端正好落在 0 与 1，也就是**接缝**上。
        //
        // 拖尾的首段（`progress = 1`）u 一路走到 0：线性过滤在 u = 0 处取 511.5 号
        // 位置，把纹理最左的纯白 (255) 和最右的纯黑 (0) 各取一半得到 127 —— sRGB
        // 解码后是 0.21，雾源凭空掉到 1/4.5。几何上的表现就是**拖尾头部**沿
        // 「垂直于笔画的直线」整片变暗：线段内 u→0 时已经掉到 233，越过端点后
        // 恒为 127，雾于是被这条直线切断，端点外那一侧再也鼓不出圆头。
        // 夹到首尾纹素中心后端点取到的就是纹素本身（纯白），雾的圆头才成立。
        // 纹理是内嵌的 512×512（见 `ba/textures.rs`），半个纹素即 1/1024。
        const float TRAIL_HALF_TEXEL = 0.5 / 512.0;
        float texel_u = clamp(
            u_progress.x + uv_along * u_progress.y,
            TRAIL_HALF_TEXEL,
            1.0 - TRAIL_HALF_TEXEL);
        vec2 trail_uv = vec2(texel_u, across);
        vec4 texel = texture2D(tex_trail, trail_uv);
        // 雾源必须沿**中心线**采样（across 钉在 0.5）：雾像素的 across 本来就
        // 在条带之外（±几十像素），拿真实 across 采样会回绕到纹理上下边缘的
        // 黑区，雾当场归零（实测 4px 外全部为 0）。上游 bloom 的种子在条带内
        // 处处非零，卷积扩散出去的雾源自然也是「亮核的颜色」，不是黑。
        vec4 fog_texel = texture2D(
            tex_trail,
            vec2(texel_u, 0.5));
        // 逐像素发射项：与上游 `TrailPixel` 的 emission 同一算式。它既是本体
        // 的发光，也是雾的 BrightPass 输入 —— 上游 bloom 的种子是**渲染出来的
        // 像素**，不是顶点色（见雾一节的说明）。
        vec3 pixel_emission = srgb_to_linear(fog_texel.rgb)
            * segment_color
            * u_intensity
            * particle_opacity
            * global_opacity;

        // 雾源还要沿笔画做一次「等价卷积」，宽度随离笔画的**距离**增长 —— 见
        // 光晕一节里 `fog_emission` 的说明（这里只保留本体需要的逐像素发射项）。

        if (body > 0.0) {
            float material_coverage = clamp(
                texel.a * particle_opacity * longitudinal * body,
                0.0,
                1.0);
            emission = pixel_emission;
            coverage = material_coverage * global_opacity;
        }

        // ── 光晕：到**整条折线**的距离 ────────────────────────────────────
        //
        // 上游是对整条笔画做整帧 mip 卷积，雾的形状是「到笔画的距离场」。逐段
        // draw 只能看到自己那一段，于是这里给每个图元额外带上前后各一段的端点，
        // 取到三段的**最小**距离 —— 那就是局部折线的距离场。
        //
        // 为什么不是别的做法（都实测过，全部有硬缺陷）：
        //   · 矩形互补斜坡：每段是一块等宽长条，雾在段端换人，被切成方块；
        //   · 只用自己的胶囊：段端收圆，相邻两段的圆头合不上，外缘扇贝波纹；
        //   · 用自己的胶囊但允许重叠：重叠区两段各出一份雾，预乘 over 得到
        //     2a - a²，轴向亮度起伏 +100%，拖尾上是一串周期亮斑；
        //   · 按 along 区间硬分割：直线段没问题，一画圈就废 —— 光晕半径 120px
        //     比曲率半径 120px 还大，切出来的是一圈放射状楔形。
        //
        // 距离场本身连续，所以每个四边形只画自己那一段的雾也不会留下缝：
        // 相邻段的胶囊在关节处重叠，`GL_MAX` 取到的两侧取值本来就相同。
        // 这里只负责把「到本段的距离」翻译成雾的强度 —— 整条笔画的并集由
        // 离屏缓冲的 max 混合完成。
        float edge_px = max(d_self - half_width, 0.0) / (ref_scale * glow_radius);
        float halo = BLOOM_TRAIL_HALO_GAIN
            * bloom_mip(edge_px, BLOOM_TRAIL_SIGMA_MAX);
        // 光晕沿用当前拖尾段的颜色，不在暗尾段额外混入白色；否则即使作者色
        // 已衰减到黑，白色光源仍会把尾巴画成一块灰雾。最终亮核的弱通道提升
        // 由下方统一的 SDR 合成完成。
        // 白混入按本段亮度取比例：亮段仍等价于混 4.5% 白，黑段一点不掺。
        //
        // **雾的源是逐像素发射项，不是顶点色。** 上游 bloom 的种子是渲染出来
        // 的像素（`bloomSeed = emission × bloomEnabled`，embedded_fx_shaders.hpp
        // 的 `TrailPixel`），mip 链对整帧卷积。之前用顶点色当源（`glow_color`
        // 沿进度从黑爬到亮青），再过一次 BrightPass：顶点色亮度在 progress
        // 0.55 处就跌破阈值 1.0，雾从中段就熄了 —— 而参考图里那一段的**核心
        // 还是饱和的**（饱和核心的亮度远超阈值，雾理应继续铺）。实测（1:1 参
        // 考，纵向 2px 剖面）：progress 0.88/0.7/0.6/0.55/0.5 处参考雾强
        // 0.86/0.63/0.33/0.29/≈0 —— 按「核心发射 − 阈值」正好落在这条曲线上。
        //
        // BrightPass（`embedded_fx_shaders.hpp:301`）作用在**原始发射亮度**上，
        // `SOURCE_SCALE` 只缩放通过门槛之后的种子 —— 先缩放再过阈值会让种子
        // 随 SOURCE_SCALE 一起被门槛吃掉（0.15 的缩放下亮度 1.4，中段直接归零）。
        //
        // **阈值必须带软膝，不能是硬门槛。** 上游的雾是 mip 链对整帧的卷积：
        // BrightPass 虽然逐像素把亮度 < 1 的种子清零，但 mip 平均后，跨越亮暗
        // 边界的块带着中间能量继续往高层漏 —— 等效于门槛被软化。逐段 draw 看
        // 不到邻居的种子，硬门槛会在「种子亮度跌破 1.0」的那条线上把雾一刀切
        // 断（纹理 RGB 在 u≈0.6 处自身衰减，种子在笔画中段就跌破阈值）：实测
        // 雾在 x≈615 处从 20 直接跳到 0，而参考图同位置还有 7~15 并平滑衰减
        // 到 x≈660。软膝让种子在亮度 0.35~1.0 之间线性淡出，雾的纵向衰减重新
        // 变成曲线，与参考一致。头段亮度 ~9.4，贡献从 0.876 升到 0.963，变化
        // 由 `SOURCE_SCALE` 的标定吸收。
        // ── 雾源：沿笔画做一次「等价卷积」（窗口宽度固定）─────────────────
        //
        // 上游的种子一进 mip 链就被平均掉了：离笔画 r 处的雾，是**半径约 r** 的
        // 核在笔画上的加权平均，而不是「最近那一点的颜色」。逐段解析近似少了
        // 这一步，雾源就完全跟着最近点的发射项走。
        //
        // 短笔画上这是致命的：`s6`（按下后只移动 5.5px）的 u 从尾端的纯黑走到
        // 头端的纯白，于是「最近点在头还是尾」把平面劈成两半 —— 头侧半圆全是
        // 亮雾、尾侧一片全黑，中间一条笔直的硬分界（复现命令：`CE_SEG=0` +
        // `s6`，实测 x=900 处 20 → 102 一像素跳变）。这正是「残留一条竖线」
        // 在短笔画上的版本。
        //
        // 用一段帐篷/矩形窗在笔画上取加权平均来近似这个卷积。
        //
        // 窗口半宽**是常量**（`BLOOM_TRAIL_FOG_BLUR_PX`，弧长像素），不随离笔画的
        // 距离增长。这一点很关键，早先写成 `clamp(edge, 8, 48)`（理由是「mip 层级
        // 随距离升高，窗口也该变宽」）会制造一圈**径向暗沟**：
        //
        //   距离带来的衰减已经全部由 `halo(edge)` 承担。再让**均值**随距离变宽，
        //   就等于把一个「越来越亮的均值」乘上一个「越来越暗的增益」。笔画尾端的
        //   种子本来就暗（纹理 u→1 是黑），窗口贴着端点张开时会逐步把笔画亮段吃进
        //   来，均值随距离明显上升 —— 净效果是靠近端点的一圈雾反而**比外侧更暗**，
        //   实测 `s7` 在端点外 0~12px 是一条 10~15 级深的新月形暗弧，正好落在
        //   `du ≈ 1`（窗口刚好盖满整条笔画）的等值线上。
        //
        // 改成常量之后窗口与距离无关，均值只随「最近点」在笔画上的位置变化，雾在
        // 径向上严格单调 —— 端点处的雾就是端点该有的值，外面平滑地衰减下去。
        // 而「短笔画被抹成均值」这个目的照样达到：`s6`（12px 笔画）在 16px 的窗口
        // 下半宽已经盖满整条笔画，均值对位置不敏感，雾就是一个圆斑。
        float fog_blur_px = BLOOM_TRAIL_FOG_BLUR_PX * ref_scale;
        // 窗口必须按**纹理 u 空间**给，不能按段参数空间。
        //
        // `u_progress` 是**全局**进度（u = 1 − progress），所以「沿笔画 blur_px」
        // 换算成 u 的偏移量是 `blur_px × |u_progress.y| / seg_len` —— 只跟段长
        // 与该段的进度跨度有关，与段被切多碎无关。
        //
        // 早先写成 `clamp(blur/seg_len, 0, 0.5)` 并夹在 `along ∈ [0,1]` 上，窗口
        // 最多只到 ±半段：短段（慢速抖动的段只有十几像素）上雾源退化成「最近
        // 那一点的颜色」，于是笔画尾端（纹理 u→0、作者色是黑）的雾被乘以黑，
        // 雾在起点处被挖出一个十几像素的暗洞，接着又跳回亮 —— 实测 `s7`
        // （60px/s，13px 笔画）在笔画起点前 x=870 掉到 25（≈ 背景），两侧却是
        // 63/78，读起来就是「雾在头部被截断」。按 u 取窗后窗口可以越过本段、
        // 取到邻居段（乃至整条笔画）的真实颜色，暗洞消失。
        //
        // 注意：「窗口越过本段、取到整条笔画的颜色」这一条曾经是靠**抽头撞边**
        // 实现的（`clamp` 到首尾纹素），那是把窗口的兜底行为当成了功能 —— 撞边
        // 同时会引入白/黑非对称，正是后面那块「平台 + 硬台阶 + 新月暗弧」的来源。
        // 现在改成归一化的中点采样，见下方卷积窗口的说明。
        float seg_len_du = max(u_extent.x - 2.0 * joint_px_len, 1.0e-3);
        float fog_du = fog_blur_px * abs(u_progress.y) / seg_len_du;
        // 退化段（进度跨度为 0）时避免 0/0：乘 0 让抽头全部落在中心。
        float inv_progress_step = abs(u_progress.y) > 1.0e-6
            ? 1.0 / u_progress.y
            : 0.0;
        float fog_u_center = u_progress.x
            + clamp(along, 0.0, 1.0) * u_progress.y;
        // ── 卷积窗口必须**归一化**，不能靠 clamp 兜底 ────────────────────
        //
        // 早先是 ±1 个抽头的三点帐篷（权重 1,2,1，除以 4），抽头位置直接
        // `clamp` 进 `[半个纹素, 1 − 半个纹素]`。那套写法在**短笔画**上会崩：
        //
        // 窗口半宽 `fog_du = blur_px / seg_len`，段长越短、离笔画越远，它就越大。
        // `s7`（60px/s，段长 11.8px）在离轴 12px 之外 `fog_du` 已经 > 1 —— 两侧
        // 抽头双双撞到纹理首尾（一端纯白、一端纯黑），中间抽头按 `along ∈ [0,1]`
        // 扫过整条纹理。于是：
        //   · 雾源沿笔画彻底失去变化，远场摊成一块**平台**（实测 y=470 一行在
        //     x=892..908 上恒为 48）；
        //   · 撞边带来的白/黑非对称在段尾 `along` 被夹回 0 处形成**竖直硬台阶**
        //     （同一行在 x=891→892 从 33 跳到 37，横跨 136 行）；
        //   · `fog_du ≈ 1` 的等值线本身是以笔画为圆心的一圈弧，跨过它时行为换
        //     档，于是雾里浮出一道**新月形暗弧**。
        // 这三样合起来就是「拖尾头部雾被截断 + 中间一道硬边 + 像被刀切掉一块」。
        //
        // 正确做法：窗口端点在 `[0,1]` 内**夹取**（`u = 1 − progress` 是全局量，
        // 整条笔画的 u 域恒为 [0,1]，夹在这里是安全的），抽头取**中点**并从窗口
        // 两端线性铺开，权重恒等归一 —— 采样点永远不会落在白端/黑端上，也就不
        // 存在非对称。窗口大于笔画时它自然退化为「整条笔画的平均」，这正是核半
        // 径 ≫ 笔画长度时的正确极限（上游 mip 链在短笔画上给出的就是一个均值、
        // 一个光滑圆斑），不再有任何平台与台阶。
        float fog_u_lo = clamp(
            fog_u_center - fog_du,
            TRAIL_HALF_TEXEL,
            1.0 - TRAIL_HALF_TEXEL);
        float fog_u_hi = clamp(
            fog_u_center + fog_du,
            TRAIL_HALF_TEXEL,
            1.0 - TRAIL_HALF_TEXEL);
        vec3 fog_source_rgb = vec3(0.0);
        for (int tap = 0; tap < FOG_TAPS; tap++) {
            // 中点采样：`f ∈ (0,1)` 恒不触到窗口端点。
            float f = (float(tap) + 0.5) / float(FOG_TAPS);
            float tap_u = mix(fog_u_lo, fog_u_hi, f);
            // 颜色渐变同样按抽头在**整条笔画**上的进度取：u = 1 − progress 是
            // 全局成立的，越过本段就是相邻段的真实进度，梯度因此连续。
            // 夹到 [0,1] 只是为了不把颜色外推成负值。
            float tap_along = clamp(
                (tap_u - u_progress.x) * inv_progress_step,
                0.0,
                1.0);
            fog_source_rgb += srgb_to_linear(texture2D(tex_trail, vec2(tap_u, 0.5)).rgb)
                * mix(u_color, u_color_b, tap_along);
        }
        fog_source_rgb /= float(FOG_TAPS);
        vec3 fog_emission = fog_source_rgb
            * u_intensity
            * particle_opacity
            * global_opacity;
        float glow_brightness = max(fog_emission.r, max(fog_emission.g, fog_emission.b));
        float glow_contribution = clamp(
            (glow_brightness - BLOOM_THRESHOLD * 0.35) / max(glow_brightness, 0.0001),
            0.0,
            1.0);
        glow_source = fog_emission * glow_contribution * BLOOM_TRAIL_SOURCE_SCALE;
        // ── 雾也必须「恰好画一次」 ────────────────────────────────────────
        //
        // 中间试过让雾走「互补斜坡」（相邻两段各画一半、权重和为 1）。方向是
        // 对的，但它和预乘 over 合成不兼容：两段各画 `w` 与 `1-w` 时得到的是
        // `1 - w(1-w)a²`，本体 alpha ≈ 1 处直接暗掉最多 25%；而且用「沿本段切线
        // 的投影」当斜坡判据时，转角两侧对同一像素的投影相差 `h · sin Δθ`
        // （h = 离折线的垂距），θ = 8°、h = 30px 就有 4px —— 两条斜坡错开，
        // 沿笔画每隔一段就是一条明暗条纹（`ce-v5/s3` 的放射亮刺与「短棒」）。
        //
        // 真正的病根不是「分割方式」，而是**同一像素被多个四边形用 over 叠加**：
        // 单段实测峰值只有 ~150，整条却饱和到 255 —— 亮度成了「这里叠了几个
        // 四边形」的函数，而四边形重叠数天然按段长周期变化。旧实现用 `owns`
        // 强行让最近段独占，代价是只能看 ±2 段、转角必然漏光。现在改用 `GL_MAX`
        // 混合（见 `render.rs` 的 `TrailTarget`）：并集律天然让每个像素只取最亮/
        // 最近的那一份，不再需要任何归属判据，也就能让每段只管自己。
        //
        // 端点**不做**任何额外收口：`d_self` 在笔画外自然增大、`halo` 自然衰减，
        // 端点外本来就是平滑的球形收尾。
        //
        // 这里曾是 `smoothstep(0, 45, 到端点距离)` 的圆形挖空，用来解决「段末
        // 朝外半平面被整块掐掉」—— 那是旧实现拿 `along_raw` 当判据留下的病。
        // 换成距离场之后不该再挖洞：`s6`（按下后只移动 5px）实测会被挖成
        // 「双峰 + 中间一条 33 的暗谷」，正是「中间有明显分割感」。上游是整帧
        // bloom 卷积，短笔画的雾是一个中心最亮的圆，绝不会在笔画上留暗缝。
        glow_shape = halo;
    }

    // ── 合成：严格对齐上游 `ResolveUnknownBackgroundDesktopTransport` ──
    // 辉光必须**在线性域**与直射色相加后再统一编码。编码是幂函数，先编码、再
    // 相加会让亮部的通道比例失真 —— 表现就是整片泛起白雾、饱和度整体下滑。
    float bloom_weight =
        max(glow_shape, 0.0) * u_bloom * element_opacity * global_opacity;
    vec3 bloom_linear = max(glow_source * bloom_weight, vec3(0.0));

    // 拖尾的雾留在本体遍里：它走离屏 `GL_MAX` 并集，整条笔画先合成成一张图
    // 再贴回来，拆成两个 pass 会破坏并集语义。
    bool trail_pass = u_mode > 2.5;

    // 不要在 `u_pass` 上开 `return` 早退：GLSL 编译器会把「只在早退分支里出现」
    // 的 uniform 判成死代码直接删掉，链接后 `getUniformLocation` 返回 -1，
    // smithay 就报 "Uniform u_pass was not declared"。改成参与 `mix` 的单出口，
    // 编译器就必须保留它。
    float glow_mix = clamp(u_pass, 0.0, 1.0);

    vec3 linear_rgb = trail_pass
        ? max(emission, vec3(0.0)) + bloom_linear
        : max(emission, vec3(0.0));
    vec3 encoded = linear_to_srgb(linear_rgb);

    // Alpha 取「视觉最大值」：光晕区域没有实体覆盖，也必须带上 Alpha，否则在
    // 浅色桌面上会被背景冲淡、在深色桌面上又退化成纯加色。
    //
    // 上限随实体覆盖度放开：上游那个 0.90 是给「网页透明叠加」预留背景透视的
    // 保守值，而 BA 在真实桌面上的圆盘是实心的（官方参考图里盘心蓝通道打满）。
    // 让实体区接近不透明、光晕区仍守在 0.90，两头都照顾到。
    float scene_coverage = clamp(coverage, 0.0, 1.0);
    // 上游 bloom 的 Alpha 通道是 `transportEnergy`：BrightPass 逐像素把
    // `max(rgb)` 写进 a，mip 链里和 RGB 同步累加。于是 bloom 的 Alpha 就是
    // **辉光自身的能量**，而不是「形状 × 增益」这个无量纲系数。
    //
    // 这里必须照同一个语义：拿 `bloom_weight` 去编码，等于让 Alpha 只跟形状
    // 走，而 `capacity = alpha / peak` 又把颜色再压一次 —— 两层相乘后光晕会被
    // 平方级压暗，薄雾直接消失（拖尾「没有辉光」就是这么来的）。
    float bloom_coverage =
        linear_to_srgb_ch(max(bloom_linear.r, max(bloom_linear.g, bloom_linear.b)));
    // 精灵的辉光已经另起加法遍，这里**不能**再让它抬高 alpha —— 那会把雾区
    // 变成一层「半透明亮膜」，按 `(1 - 雾亮度)` 压掉背景，环带两侧的层次全被
    // 抹平。这是「发光把整个环糊住」的直接原因，也是与上游最大的结构差异。
    float glow_alpha = trail_pass ? bloom_coverage : 0.0;
    float alpha_limit = mix(TRANSPORT_ALPHA_LIMIT, 1.0, scene_coverage);
    float alpha = min(max(scene_coverage, glow_alpha), alpha_limit);

    // 容量压缩：预乘色不允许超过 Alpha。这条不变量让源覆盖混合既能提亮（亮于
    // 背景）也能压暗（暗于背景），观感与上游 DComp 输出一致。
    float peak = max(encoded.r, max(encoded.g, encoded.b));
    float capacity = min(1.0, alpha / max(peak, 0.000001));
    vec3 premultiplied = min(encoded * capacity, vec3(alpha));
    // **拖尾豁免容量压缩**。
    //
    // 上游 `TrailPixel`（embedded_fx_shaders.hpp:156）写得很直白：
    // "Desktop Coverage is transport metadata. Unity emission must not be dimmed
    // by it." —— `output.direct = (emission, coverage)`，RGB 是**未乘覆盖度**的
    // 发射项，混合时 `dst = emission + dst × (1-coverage)`。纵向淡出只进 Alpha，
    // 不进 RGB。
    //
    // 容量压缩等价于把 emission 乘 `coverage/peak`：拖尾的纵向覆盖从 progress
    // 0.2485 一路爬到 0.98，中段只有 0.4 —— 于是实体亮核被覆盖度整体压暗，
    // 实测 progress 0.59 处从应到的 ~233 掉到 86、progress 0.54 处只剩 40，
    // 拖尾可见长度只有官方参考图的一半（饱和段 280..520 vs 参考 280..680，
    // 尾部 640 处参考还有 255、我们只剩 40）。
    //
    // 圆盘/环/碎片不豁免：它们的核心覆盖度本来就近乎 1（capacity≈1，压缩是
    // 恒等变换），而光晕区 emission≈0，压缩与否无感。只有拖尾是「高发射 +
    // 低覆盖」的长条，才会被这条不变量伤到。
    if (u_mode > 2.5) {
        premultiplied = encoded;
    }

    // bright-core 补偿：把亮核的弱通道抬向峰值，重演上游「核心发白、外圈留色」
    // 的观感。能量低的拖尾尾段被 gate 挡住，不会整条尾巴灰掉。
    //
    // 强度取 0.10 而不是上游的 0.35：上游是为「不知道背景 + 网页叠加」调的，
    // 会把圆盘从 (78,138,229) 抬成 (137,169,229) 的灰蓝；按官方参考图的盘心
    // 实测 (108,163,255) 反解，0.10 才对得上。
    //
    // **拖尾不参与**：作者色 `evaluateUnityTrailColor` 的 R 恒为 0，官方图里整条
    // 亮带的 R 也始终等于背景（28 vs 25）。补偿会把 R 抬向峰值，实测把亮带从
    // (28,255,255) 抬成 (50,255,255) —— 纯青变发白，色相整个偏掉。圆盘和环本身
    // 就带红（盘心 114），补偿对它们才成立。
    float premultiplied_peak = max(
        premultiplied.r,
        max(premultiplied.g, premultiplied.b));
    float energy_ratio = premultiplied_peak / max(alpha, 0.000001);
    float gate = smoothstep(0.25, 0.75, energy_ratio)
        * smoothstep(0.03125, 0.25, premultiplied_peak)
        * smoothstep(0.05, 0.5, scene_coverage)
        * (u_mode < 2.5 ? 1.0 : 0.0);
    premultiplied = mix(
        premultiplied,
        vec3(premultiplied_peak),
        TRANSPORT_COMPENSATION_MIX * gate);

    // 本体遍（预乘 over）与辉光遍（`alpha = 0` ⇒ 等效 `One/One` 加法）在这里
    // 合一。选 `alpha = 0` 而不是换混合状态，是因为 smithay 每画一个图元都会
    // 重设 `glBlendFunc`，外面设不进去。
    vec4 body = vec4(premultiplied, alpha);
    vec4 glow = vec4(linear_to_srgb(bloom_linear), 0.0);
    gl_FragColor = mix(body, glow, glow_mix) * niri_alpha;

#if defined(DEBUG_FLAGS)
    if (niri_tint == 1.0)
        gl_FragColor = vec4(0.0, 0.3, 0.0, 0.3) + gl_FragColor * 0.7;
#endif
}
