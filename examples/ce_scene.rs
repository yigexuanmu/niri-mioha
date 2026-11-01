//! 固定场景的离线渲染器（迁移计划阶段 B/阶段 C 的工具）。
//!
//! 走的是和实机完全相同的链路：`CursorEffectState` 喂指针事件 → `build_commands`
//! → `ShaderRenderElement` + `cursor_effect.frag` → 预乘合成到离屏 FBO。
//! 唯一差别是最后用 `ReadPixels` 把结果回读成 PPM。
//!
//! 用法：
//! ```text
//! ce_scene <scene> <outdir> [iso] [bg] [kdl]
//!   scene : s1 | s2 | s3 | s4 | s5 | demo | all
//!   iso   : 额外导出按材质隔离的单层图（disk/ring/tri/trail）
//!   bg    : dark（默认，(5,8,20)）| black
//!   kdl   : 可选，一个 niri 配置片段（写 `cursor-effect { ... }`），
//!           走的是 `settings_from_config` 同一条映射路径 —— 用来验证
//!           「用户改配置 → 画面确实变了」，而不只是「配置能解析」。
//! ```
//!
//! 每帧导出 `<scene>/<t>.ppm`；隔离图导出 `<scene>/<t>.<material>.ppm`。

use std::fs;
use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use niri::cursor_effect::ba::{Point as FxPoint, Viewport};
use niri::cursor_effect::render::{collect_render_elements, CursorEffectElement, Material};
use niri::cursor_effect::state::{CursorEffectSettings, CursorEffectState};
use niri::cursor_effect::settings_from_config;
use niri::render_helpers::{resources, shaders};
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::gles::{ffi, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Color32F, Frame, FrameContext, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Scale, Size, Transform};

/// 设计基准分辨率：下面所有场景坐标都是按 1920×1080 写的。
const W: i32 = 1920;
const H: i32 = 1080;

/// 实际渲染尺寸，`CE_W` / `CE_H` 覆盖，默认就是设计基准。
///
/// 为什么必须能改：特效自身的尺寸不是固定像素，而是
/// `reference_scale = viewport.height / 1080`（见 `ba::reference_scale`）。
/// 实机输出 2560×1600 的 ref_scale 是 1.4815，离线按 1080p 渲就是 1.0 ——
/// 同一个特效两边差 1.48 倍像素，拿来做逐像素对比必然「不相符」。
/// 想和实机截图 1:1 叠加，就用实机的尺寸渲染。
fn out_size() -> (i32, i32) {
    static CACHE: std::sync::OnceLock<(i32, i32)> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        let read = |key: &str, default: i32| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<i32>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        };
        (read("CE_W", W), read("CE_H", H))
    })
}

/// 场景坐标 / 运动速度相对设计基准的放大倍数（= 输出高度 / 1080）。
fn out_scale() -> f32 {
    out_size().1 as f32 / H as f32
}

/// 上游 `TRAIL_LIFETIME_SECONDS`：拖尾在身后的存活时长。
const TRAIL_LIFETIME: f64 = 0.30;

// ── 折线 / 弧长参数化 ───────────────────────────────────────────────────────

struct Path {
    pts: Vec<(f32, f32)>,
    cum: Vec<f32>,
    total: f32,
}

impl Path {
    fn from_points(pts: Vec<(f32, f32)>) -> Self {
        let mut cum = vec![0.0f32];
        for pair in pts.windows(2) {
            let d = ((pair[1].0 - pair[0].0).powi(2) + (pair[1].1 - pair[0].1).powi(2)).sqrt();
            cum.push(cum.last().unwrap() + d);
        }
        let total = *cum.last().unwrap_or(&0.0);
        Self { pts, cum, total }
    }

    /// Catmull-Rom 加密后的平滑折线。
    fn from_spline(ctrl: &[(f32, f32)], per_seg: usize) -> Self {
        let n = ctrl.len();
        let mut out = Vec::new();
        for i in 0..n - 1 {
            let p0 = ctrl[i.saturating_sub(1)];
            let p1 = ctrl[i];
            let p2 = ctrl[i + 1];
            let p3 = ctrl[(i + 2).min(n - 1)];
            for s in 0..per_seg {
                let t = s as f32 / per_seg as f32;
                let t2 = t * t;
                let t3 = t2 * t;
                let f = |a: f32, b: f32, c: f32, d: f32| {
                    0.5 * (2.0 * b
                        + (-a + c) * t
                        + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                        + (-a + 3.0 * b - 3.0 * c + d) * t3)
                };
                out.push((f(p0.0, p1.0, p2.0, p3.0), f(p0.1, p1.1, p2.1, p3.1)));
            }
        }
        out.push(*ctrl.last().unwrap());
        Self::from_points(out)
    }

    fn at(&self, s: f32) -> (f32, f32) {
        if self.pts.len() < 2 {
            return self.pts.first().copied().unwrap_or((0.0, 0.0));
        }
        let s = s.clamp(0.0, self.total);
        // 二分找段。
        let mut lo = 0usize;
        let mut hi = self.cum.len() - 1;
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.cum[mid] <= s {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let seg = self.cum[lo + 1] - self.cum[lo];
        let f = if seg > 1e-6 {
            (s - self.cum[lo]) / seg
        } else {
            0.0
        };
        let a = self.pts[lo];
        let b = self.pts[lo + 1];
        (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f)
    }

    /// 从 `center` 起画一段圆弧（角度单位：度，逆时针为正）。
    fn arc(center: (f32, f32), radius: f32, from_deg: f32, sweep_deg: f32) -> Self {
        let steps = ((sweep_deg.abs() / 1.5).ceil() as usize).max(8);
        let mut pts = Vec::with_capacity(steps + 1);
        for i in 0..=steps {
            let a = (from_deg + sweep_deg * i as f32 / steps as f32).to_radians();
            pts.push((center.0 + radius * a.cos(), center.1 + radius * a.sin()));
        }
        Self::from_points(pts)
    }
}

// ── 场景定义 ────────────────────────────────────────────────────────────────

struct Scene {
    name: &'static str,
    path: Path,
    /// 指针沿路径前进的速度（px/s）。0 表示静止。
    speed: f32,
    press_at: Option<f64>,
    release_at: Option<f64>,
    end: f64,
    /// 出图时刻（秒，相对场景起点）。
    samples: Vec<f64>,
}

impl Scene {
    fn pos(&self, t: f64) -> (f32, f32) {
        // 路径按 1080p 设计，输出更大时整条轨迹（含走了多远）等比放大，
        // 这样「屏幕上占多少比例」与实机一致，拖尾长度也才一致。
        let s = out_scale();
        let (x, y) = self.path.at((self.speed as f64 * t) as f32);
        (x * s, y * s)
    }
}

fn scenes() -> Vec<Scene> {
    // S1 静止点击：单独验证圆盘 / 环 / 碎片。
    // 采样加密到环的整个寿命（600ms），用来和官方参考图按「环半径」对齐比较。
    let s1 = Scene {
        name: "s1",
        path: Path::from_points(vec![(960.0, 540.0), (960.0, 541.0)]),
        speed: 0.0,
        press_at: Some(0.0),
        release_at: None,
        end: 0.60,
        samples: (2..=30).map(|i| i as f64 * 0.02).collect(),
    };

    // S2 直线拖动：与官方参考图构图一致（按下后横拖 ~435px，盘仍在盘心）。
    let s2_path = Path::from_points(vec![(760.0, 540.0), (2000.0, 540.0)]);
    let s2 = Scene {
        name: "s2",
        speed: 2900.0,
        press_at: Some(0.15),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![0.160, 0.180, 0.200, 0.220, 0.250, TRAIL_LIFETIME],
        path: s2_path,
    };

    // S3 画圈：半径 120px，一圈 300°（留下一道开口便于看接缝）。
    let s3_path = Path::arc((960.0, 540.0), 120.0, -90.0, 300.0);
    let s3_speed = (s3_path.total as f64 / TRAIL_LIFETIME) as f32;
    let s3 = Scene {
        name: "s3",
        speed: s3_speed,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![TRAIL_LIFETIME],
        path: s3_path,
    };

    // S4 急折返：两次 150° 转角。
    let s4_path = Path::from_points(vec![
        (620.0, 560.0),
        (1080.0, 560.0),
        (716.0, 770.0),
        (916.0, 424.0),
    ]);
    let s4_speed = (s4_path.total as f64 / TRAIL_LIFETIME) as f32;
    let s4 = Scene {
        name: "s4",
        speed: s4_speed,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![TRAIL_LIFETIME],
        path: s4_path,
    };

    // S5 慢速直线：400px/s，验证低速下带宽与亮度。
    let s5_path = Path::from_points(vec![(700.0, 540.0), (1200.0, 540.0)]);
    let s5 = Scene {
        name: "s5",
        speed: 400.0,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![TRAIL_LIFETIME],
        path: s5_path,
    };

    // demo 自由曲线：交付用的演示图。
    let demo_path = Path::from_spline(
        &[
            (430.0, 640.0),
            (560.0, 740.0),
            (740.0, 730.0),
            (860.0, 600.0),
            (830.0, 450.0),
            (680.0, 400.0),
            (540.0, 470.0),
            (540.0, 610.0),
            (660.0, 690.0),
            (820.0, 660.0),
            (960.0, 560.0),
        ],
        60,
    );
    let demo_speed = (demo_path.total as f64 / TRAIL_LIFETIME) as f32;
    let demo = Scene {
        name: "demo",
        speed: demo_speed,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![0.05, 0.10, 0.15, 0.20, 0.25, TRAIL_LIFETIME],
        path: demo_path,
    };

    // S6 按下后只移动一丁点（12px。TRAIL_LIFETIME 内只走 ~10px）：
    // 复现「长按微移」时光晕只剩一侧半圆、中间一条竖直硬边的问题。
    let s6_path = Path::from_points(vec![(900.0, 500.0), (912.0, 504.0)]);
    let s6 = Scene {
        name: "s6",
        speed: 30.0,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![0.10, 0.20, TRAIL_LIFETIME],
        path: s6_path,
    };

    // R1 与官方参考图 1 同构：从右往左拖 ~815px，笔画落在 0.3s 寿命的一整段里，
    // 可见部分（progress > 0.2485）约 615px，头部在左端。
    let r1_path = Path::from_spline(
        &[
            (1080.0, 205.0),
            (880.0, 196.0),
            (650.0, 178.0),
            (400.0, 180.0),
            (265.0, 205.0),
        ],
        90,
    );
    let r1_speed = (r1_path.total as f64 / TRAIL_LIFETIME) as f32;
    let r1 = Scene {
        name: "r1",
        speed: r1_speed,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![0.05, 0.10, 0.15, 0.20, 0.25, TRAIL_LIFETIME],
        path: r1_path,
    };

    // S7 慢速直线（60px/s，0.3s 只走 18px）：复现「慢慢拖几个像素」。
    // 采样加密到每 20ms 一帧，用来量测同一世界坐标上的亮度随时间是否抖动。
    let s7_path = Path::from_points(vec![(880.0, 520.0), (980.0, 520.0)]);
    let s7 = Scene {
        name: "s7",
        speed: 60.0,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: (5..=15).map(|i| i as f64 * 0.02).collect(),
        path: s7_path,
    };

    // S8 慢速抖动（60px/s + 垂直于运动方向 0.7px 的正弦摆动，波长 ~5px）：
    // 复现「一顿一顿地拖」。这种轨迹上 `merge_collinear_runs` 的弓高判据会在
    // 0.6px 附近反复翻转，段被切得很碎 —— 正是雾闪烁最容易暴露的情形。
    let mut s8_pts = Vec::new();
    for i in 0..=120 {
        let x = 880.0 + i as f32 * 1.0;
        let y = 520.0 + (i as f32 * 1.25).sin() * 0.7;
        s8_pts.push((x, y));
    }
    let s8_path = Path::from_points(s8_pts);
    let s8 = Scene {
        name: "s8",
        speed: 60.0,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: (5..=15).map(|i| i as f64 * 0.02).collect(),
        path: s8_path,
    };

    // S9 快速往返（4000px/s 的 6 段折返，往返间隔 15px）：复现「快速拖动时
    // 雾被截断和重叠」。相邻两臂间距只有 15px，远小于 120px 的光晕半径 ——
    // 旧实现「本段 + ±2 段」的距离窗口看不到隔了两段的那一臂，于是在臂间留下
    // 楔形缺口；同时非相邻四边形的 halo 又会以 over 叠成亮块。`GL_MAX` 并集
    // 下这两件事都不会发生（见 `render.rs` 的 `TrailTarget`）。
    let s9_path = Path::from_points(vec![
        (700.0, 500.0),
        (900.0, 500.0),
        (700.0, 515.0),
        (900.0, 515.0),
        (700.0, 530.0),
        (900.0, 530.0),
    ]);
    let s9 = Scene {
        name: "s9",
        speed: 4000.0,
        press_at: Some(0.0),
        release_at: None,
        end: TRAIL_LIFETIME,
        samples: vec![0.20, 0.24, 0.28],
        path: s9_path,
    };

    vec![s1, s2, s3, s4, s5, s6, s7, s8, s9, demo, r1]
}

// ── 渲染 ────────────────────────────────────────────────────────────────────

fn render_frame(
    renderer: &mut GlesRenderer,
    elems: &[CursorEffectElement],
    filter: Option<Material>,
    bg: [f32; 3],
    out: &FsPath,
) -> anyhow::Result<()> {
    let (w, h) = out_size();
    let size = Size::<i32, BufferCoord>::from((w, h));
    let mut target: GlesTexture = renderer
        .create_buffer(smithay::backend::allocator::Fourcc::Abgr8888, size)
        .context("create_buffer")?;
    let damage = [Rectangle::from_size(Size::<i32, Physical>::from((w, h)))];
    let mut pixels = vec![0u8; (w * h * 4) as usize];

    {
        let mut fb = renderer.bind(&mut target).context("bind")?;
        let mut frame = renderer
            .render(&mut fb, Size::from((w, h)), Transform::Normal)
            .context("render")?;
        frame
            .clear(
                Color32F::from([bg[0], bg[1], bg[2], 1.0]),
                &damage,
            )
            .context("clear")?;

        // 反向迭代：`collect_render_elements` 按 niri 的约定返回**从前到后**
        // （栈顶在前）的元素表，niri 在落笔前同样 `.iter().rev()`（见
        // `Niri::screenshot` 等）。这里必须用同一套顺序，否则离线图与实机会
        // 上下颠倒 —— 那正是「同一份代码，碎片一个在最底一个在最顶」的由来。
        //
        // `CE_ORDER=fwd` 保留修复前的行为（按表原顺序画），用来复现旧实机图。
        let forward = std::env::var("CE_ORDER").is_ok_and(|v| v == "fwd");
        let iter: Box<dyn Iterator<Item = &CursorEffectElement>> = if forward {
            Box::new(elems.iter())
        } else {
            Box::new(elems.iter().rev())
        };
        for elem in iter {
            if let Some(m) = filter {
                if elem.material() != Some(m) {
                    continue;
                }
            }
            let src = elem.src();
            let dst = elem.geometry(Scale::from(1.0));
            if std::env::var("CE_GLDBG").is_ok() {
                let mut guard = frame.renderer();
                let renderer = guard.as_mut();
                let mut eq = [0i32; 1];
                renderer.with_context(|gl| unsafe {
                    gl.GetIntegerv(ffi::BLEND_EQUATION_RGB, eq.as_mut_ptr());
                })?;
                println!("  eq=0x{:x} (MAX=0x8008, ADD=0x8006) for {:?}", eq[0], elem.material());
            }
            RenderElement::<GlesRenderer>::draw(elem, &mut frame, src, dst, &damage, &[], None)
                .context("draw")?;
        }

        {
            let mut guard = frame.renderer();
            let renderer = guard.as_mut();
            renderer.with_context(|gl| unsafe {
                gl.ReadPixels(
                    0,
                    0,
                    w,
                    h,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    pixels.as_mut_ptr() as *mut _,
                );
            })?;
        }
        frame.finish().context("finish")?;
    }

    // 不再翻转 Y：smithay 的 GL 帧已经是 y 向下的投影（与元素坐标同向），
    // 回读的行序就是元素坐标的行序。早先这里多做了一次翻转，导致离线分析
    // 一直在镜像坐标上做，越查越乱。
    let mut ppm = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        let row = &pixels[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
        for px in row.chunks_exact(4) {
            ppm.extend_from_slice(&px[0..3]);
        }
    }
    let mut file = format!("P6\n{w} {h}\n255\n").into_bytes();
    file.extend_from_slice(&ppm);
    fs::write(out, file)?;
    Ok(())
}

fn material_tag(m: Material) -> &'static str {
    match m {
        Material::Disk => "disk",
        Material::Ring => "ring",
        Material::Triangle => "tri",
        Material::Trail => "trail",
    }
}

fn run_scene(
    renderer: &mut GlesRenderer,
    scene: &Scene,
    outdir: &FsPath,
    iso: bool,
    bg: [f32; 3],
    settings: Option<CursorEffectSettings>,
) -> anyhow::Result<()> {
    let dir = outdir.join(scene.name);
    fs::create_dir_all(&dir)?;

    let mut state = CursorEffectState::default();
    let (vw, vh) = out_size();
    state.set_viewport(Viewport::new(vw as u32, vh as u32));
    if let Some(settings) = settings {
        state.apply_settings(settings);
    }
    let start = Instant::now();
    // 把状态内部的时间原点锚到本场景的时钟：否则 epoch（真实创建时刻）与本
    // 时钟之间夹着的创建耗时会带纳秒级运行间抖动，40Hz 输入采样门在 0.025s
    // 边界上的判定随机翻转，拖尾末端凭空差出 2~3 个采样点。
    state.set_time_origin(start);
    let now_at = |t: f64| start + Duration::from_secs_f64(t);

    // 事件时间表（按下 / 抬起），与采样时刻共用同一套步进。
    let mut events: Vec<(f64, bool)> = Vec::new();
    if let Some(t) = scene.press_at {
        events.push((t, true));
    }
    if let Some(t) = scene.release_at {
        events.push((t, false));
    }
    events.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut samples = scene.samples.clone();
    samples.sort_by(|a, b| a.total_cmp(b));

    const DT: f64 = 1.0 / 240.0;
    const EPS: f64 = 1e-9;

    let mut t = 0.0f64;
    let mut ev = 0usize;
    let mut si = 0usize;

    loop {
        // 1) 处理恰在 t 的事件（按下 / 抬起）
        while ev < events.len() && events[ev].0 <= t + EPS {
            let (x, y) = scene.pos(t);
            if events[ev].1 {
                state.on_button_press(
                    FxPoint::new(x, y),
                    now_at(t),
                    niri::cursor_effect::TriggerButton::Left,
                );
            } else {
                state.on_button_release(now_at(t));
            }
            ev += 1;
        }

        // 2) 指针位置 + 推进一帧
        let (x, y) = scene.pos(t);
        state.on_move(FxPoint::new(x, y), now_at(t));
        state.advance(now_at(t));
        state.on_frame_rendered(now_at(t));

        // 3) 到点出图
        while si < samples.len() && samples[si] <= t + EPS {
            let ts = samples[si];
            si += 1;
            let mut elems = collect_render_elements(renderer, &state, now_at(ts), 1.0);
            // CE_SEG=N：只画第 N 个拖尾段，其余全丢。用来逐段检查形状，
            // 判断「拖尾断成一节节」到底是每段自身收缩，还是段与段之间漏画。
            //
            // 拖尾现在在 `collect_render_elements` 里就合成成**一个**离屏元素，
            // 所以按段过滤已经挪进库里（同样是 `CE_SEG`，直接决定哪一段进离屏
            // 缓冲）。这里只剩「丢掉非拖尾元素」这一半职责。
            if std::env::var("CE_SEG").is_ok() {
                elems.retain(|e| e.material() == Some(Material::Trail));
            }
            let mut counts = [0usize; 4];
            for e in &elems {
                if let Some(m) = e.material() {
                    counts[m as usize] += 1;
                }
            }
            let composite = dir.join(format!("{ts:.3}.ppm"));
            // CE_ONLY=disk|ring|triangle|trail：只保留某一类材质，用来分离
            // 「多层 over 合成」与「单层自身输出」的差异。
            if let Ok(only) = std::env::var("CE_ONLY") {
                elems.retain(|e| {
                    e.material()
                        .map(|m| material_tag(m) == only)
                        .unwrap_or(false)
                });
            }
            // CE_FIRST=N：只保留「绘制顺序」的前 N 个元素（前缀），用于二分
            // 「哪一个元素加入之后合成结果变了」。元素表是从前到后的，所以绘制
            // 顺序的前缀 = 表尾的 N 个。
            if let Ok(n) = std::env::var("CE_FIRST") {
                let n: usize = n.parse().unwrap_or(usize::MAX);
                let drop = elems.len().saturating_sub(n);
                elems.drain(..drop);
            }
            if std::env::var("CE_DUMP").is_ok() {
                for e in &elems {
                    let g = e.geometry(Scale::from(1.0));
                    println!(
                        "    {:?} quad=({},{} {}x{})",
                        e.material(),
                        g.loc.x,
                        g.loc.y,
                        g.size.w,
                        g.size.h
                    );
                }
            }
            render_frame(renderer, &elems, None, bg, &composite)?;
            println!(
                "{} t={ts:.3} elements={} disk={} ring={} tri={} trail={} -> {}",
                scene.name,
                elems.len(),
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                composite.display()
            );
            if iso {
                for m in [
                    Material::Disk,
                    Material::Ring,
                    Material::Triangle,
                    Material::Trail,
                ] {
                    if counts[m as usize] == 0 {
                        continue;
                    }
                    let p = dir.join(format!("{ts:.3}.{}.ppm", material_tag(m)));
                    render_frame(renderer, &elems, Some(m), bg, &p)?;
                }
            }
        }

        // 4) 走到下一个边界（dt / 事件 / 采样，取最近者）
        if t >= scene.end - EPS {
            break;
        }
        let mut t_next = (t + DT).min(scene.end);
        if let Some(&(te, _)) = events.get(ev) {
            // 只取严格晚于当前时刻的边界：te <= t 的事件会在下一轮 while 里
            // 消费，这里若取到它会让 t_next == t。
            if te > t && te < t_next {
                t_next = te;
            }
        }
        if let Some(&ts) = samples.get(si) {
            if ts > t && ts < t_next {
                t_next = ts;
            }
        }
        // 吸附到 1/240 网格：连加 DT 会积累浮点漂移，t=0.300 附近的采样门判定
        // （`t - 上次采样 < 1/40`）会随机翻转。事件与采样时刻都是 1/240 的整数
        // 倍，吸附是无损的，且让所有时刻都是精确的 f64。
        let snapped = (t_next * 240.0).round() / 240.0;
        // 兜底：任何情况下都必须严格前进，否则死循环（s1 场景曾在此卡死
        // 28 分钟 CPU 满转——t_next 吸附回 t 本身）。
        t = if snapped > t { snapped } else { t + DT };
    }
    Ok(())
}

fn parse_bg(s: &str) -> [f32; 3] {
    match s {
        "black" => [0.0, 0.0, 0.0],
        _ => [5.0 / 255.0, 8.0 / 255.0, 20.0 / 255.0],
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let which = args.get(1).cloned().unwrap_or_else(|| "s1".into());
    let outdir = PathBuf::from(
        args.get(2)
            .cloned()
            .unwrap_or_else(|| "/tmp/ce-golden".into()),
    );
    let iso = args.get(3).map(|s| s == "iso").unwrap_or(false);
    let bg = parse_bg(args.get(4).map(String::as_str).unwrap_or("dark"));
    let settings = match args.get(5) {
        Some(path) => {
            let text = fs::read_to_string(path)
                .with_context(|| format!("read config fragment {path}"))?;
            let config = niri_config::Config::parse_mem(&text)
                .with_context(|| format!("parse config fragment {path}"))?;
            Some(settings_from_config(&config.cursor_effect))
        }
        None => None,
    };

    let mut renderer = unsafe {
        let display = EGLDisplay::new(EGLSurfacelessDisplay).context("EGL display")?;
        let context = EGLContext::new(&display).context("EGL context")?;
        GlesRenderer::new(context).context("GlesRenderer")?
    };
    resources::init(&mut renderer);
    shaders::init(&mut renderer);

    let all = scenes();
    let picked: Vec<&Scene> = if which == "all" {
        all.iter().collect()
    } else {
        let s = all
            .iter()
            .find(|s| s.name == which)
            .with_context(|| format!("unknown scene {which}"))?;
        vec![s]
    };

    for scene in picked {
        run_scene(&mut renderer, scene, &outdir, iso, bg, settings)?;
    }
    Ok(())
}
