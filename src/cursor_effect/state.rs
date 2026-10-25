//! 光标特效纯逻辑状态机（Cursor Effects pure-logic state machine）。
//!
//! 粒子状态（点击爆裂 / 拖尾 / 滚轮圆环 / 火花）的物理与动画推进逻辑，不依赖
//! GLES 与 niri 渲染，可在单元测试中独立驱动。渲染（`render` 模块）只负责把状态
//! 翻译成字符粒子绘制。
//!
//! 参考了 BASpark（`DoomVoss/BASpark`）的粒子概念，但数值与行为已大量自定。

use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};
use std::time::Instant;

// ────────────────────────── 硬参数常量（按特效种类分组）──────────────────────────

/// 点击爆裂圆盘（filledCircle）配置
pub mod filled_cfg {
    pub const R_ADD_RATE: f32 = 26.0;
    pub const MAX_LIFE: f32 = 16.0;
    /// 淡出窗口延长系数：半径/运动仍由 MAX_LIFE 驱动，淡出 alpha 用
    /// `life / (MAX_LIFE * FADE_EXTEND)` → 圆盘膨胀速度不变，消失更慢、
    /// 各格错开的淡出时间更明显。
    pub const FADE_EXTEND: f32 = 4.0;
}

/// 点击爆裂外圈环（rings）配置
pub mod rings_cfg {
    pub const RS_LIST: [f32; 3] = [0.0, 0.08, 0.1];
    pub const R_ROUND_RATE_LIST: [f32; 4] = [0.0, 1.0, 1.5, 2.0];
    pub const LEN: f32 = 1.1 * std::f32::consts::PI;
    pub const MAX_LIFE: f32 = 23.0;
    /// 同上：环的淡出/收缩更慢，位置运动不变；延长后各字符独立消失时间更明显。
    pub const FADE_EXTEND: f32 = 4.0;
    pub const SEG_NUM: usize = 16;
    pub const MIN_W: f32 = 0.4;
    pub const MAX_W: f32 = 3.3;
    pub const LEN_STOP_ADD_POINT: f32 = 0.1;
    pub const LEN_START_DIM_POINT: f32 = 0.4;
}

/// 点击爆裂生成配置
pub mod create_click_cfg {
    pub const RINGS_RS_LIST: [f32; 3] = [0.0, 0.03, 0.06];
    pub const RINGS_R_ROUND_RATE_LIST: [f32; 4] = [0.0, 1.0, 1.5, 2.0];
    pub const RINGS_LEN: f32 = 1.1 * std::f32::consts::PI;
    pub const SPARKS_COUNT: usize = 8;
}

/// 滚轮代码圆环配置（自定特效）。
pub mod scroll_cfg {
    /// 圆环半径（scale=1.5 时），渲染时按 scale/1.5 缩放。
    pub const RADIUS: f32 = 62.0;
    /// 圆环字符数。
    pub const CHARS: usize = 22;
    /// 每整档滚轮（120 v120 单位）注入的旋转角（rad）：滚轮越快 → 单位时间
    /// 旋转角越大，转动速度跟随滚轮速度。
    pub const ANGLE_PER_NOTCH: f32 = 0.5;
    /// 停止滚动后继续"惯性"旋转的帧数（逐帧递减）。
    pub const ROT_STOP_FRAMES: u16 = 12;
    /// 惯性旋转的最大角速度（rad/帧，随剩余帧数线性衰减）。
    pub const COAST_ANGLE: f32 = 0.12;
    /// 停止滚动后保持满亮的帧数，随后开始逐个淡出。
    pub const GRACE_FRAMES: u16 = 14;
    /// 淡出窗口（帧数）：`GRACE_FRAMES` 之后在这个窗口内逐个消失。
    pub const FADE_FRAMES: f32 = 52.0;
    /// 同时存在的环数量上限（滚动极快时兜底）。
    pub const MAX_RINGS: usize = 6;
    /// 与已有环"合并刷新"的最大距离（px）：同向且相近 → 延续同一条环。
    pub const MERGE_DIST: f32 = 60.0;
    /// 下滚（顺时针）淡橙。
    pub const COLOR_DOWN: [f32; 3] = [1.0, 190.0 / 255.0, 118.0 / 255.0];
    /// 上滚（逆时针）淡绿。
    pub const COLOR_UP: [f32; 3] = [158.0 / 255.0, 1.0, 150.0 / 255.0];
}

/// 帧率归一参数（基准 60Hz，delta 上限 100ms）
pub const BASE_FRAME_MS: f32 = 1000.0 / 60.0;
pub const MAX_DELTA_MS: f32 = 100.0;

// ────────────────────────── 随机源 ──────────────────────────

/// 注入式随机源：生产用 fastrand（非确定性），测试用 SequentialRng（确定性）。
pub trait EffectRng {
    /// 返回 [0, 1) 的 f32。
    fn f32(&mut self) -> f32;
}

/// 稳定伪随机数 [0,1)：同一 seed 恒返回同一值（实体生命周期内恒定），
/// 用于给每个字符/火花派生独立的速度/半径/大小/淡出时间 → 不规则运动与消失。
pub fn hash_unit(seed: usize) -> f32 {
    let mut h = (seed as u64).wrapping_mul(0x9E3779B97F4A7C15);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58476D1CE4E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D049BB133111EB);
    h ^= h >> 31;
    ((h & 0xFF_FFFF) as f32) / 16_777_216.0
}

/// 生产随机源：基于 fastrand（niri 已依赖 fastrand 2.4.1）。
#[derive(Debug, Default)]
pub struct FastrandRng;
impl EffectRng for FastrandRng {
    #[inline]
    fn f32(&mut self) -> f32 {
        fastrand::f32()
    }
}

// ────────────────────────── 数据结构（粒子实体）──────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct RingSeg {
    pub off: f32,
    pub len: f32,
    pub r_round_rate: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct Ring {
    pub ang: f32,
    pub rs: f32,
    pub segs: [RingSeg; 2],
}

#[derive(Debug, Clone, Copy)]
pub struct Wave {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub life: f32,
    pub ring: Ring,
    /// 每次点击随机生成的种子：让环/圆盘每个字符的速度、半径、淡出时间
    /// 每次点击都不同（否则由弧位置 k 派生 → 每次点击几乎一样）。
    pub seed: u64,
    /// 爆裂颜色（创建时快照，避免被后续点击的按键颜色覆盖）。
    pub color: [f32; 3],
    /// 环的末端颜色（创建时快照）。
    pub rings_end_color: [f32; 3],
}

#[derive(Debug, Clone, Copy)]
pub struct Fragment {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub rot: f32,
    pub rs: f32,
    pub s: f32,
    pub a: f32,
    pub f: f32,
    pub from_click: bool,
    /// 颜色快照（创建时），保证不同按键的爆裂/拖尾可同时渲染。
    pub color: [f32; 3],
}

#[derive(Debug, Clone, Copy)]
pub struct TrailPoint {
    pub x: f32,
    pub y: f32,
    pub life: f32,
    /// 稳定标识（创建时递增分配），用作字符闪烁的随机种子——保证字符只在
    /// 寿命期内的固定时间点变化，而不是随队列 index 位移而抖动。
    pub id: u64,
    /// 颜色快照（创建时），让旧拖尾保持原按键颜色。
    pub color: [f32; 3],
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollRing {
    pub x: f32,
    pub y: f32,
    /// 旋转方向：+1 顺时针（滚轮向下，淡橙），-1 逆时针（滚轮向上，淡绿）。
    pub dir: f32,
    /// 颜色快照（创建时按方向固定）。
    pub color: [f32; 3],
    /// 累计旋转角（rad）。由滚轮位移注入，停止滚动后短暂惯性衰减。
    pub ang: f32,
    /// 上一帧的 `ang` 快照：差值 = 本帧扫过的角 → 决定甩出火花的力度。
    pub last_ang: f32,
    /// 自上次滚动 tick 以来的帧数；滚动 tick 会归零刷新。
    pub idle: u16,
    /// 滚轮旋转速率（rad/帧）= 注入角 ÷ 距上一滚动事件的帧数：慢滚 → 低速率，
    /// 快滚 → 高速率。甩出火花的速度 ∝ 该速率（而非每个事件固定的注入角）。
    pub ang_rate: f32,
    /// 随机种子（外观：字符淡出窗口、大小）。
    pub seed: u64,
    /// 甩出火花计数器（确定性伪随机用）。
    pub spark_sn: u64,
}

/// 点击触发按键：左键 / 右键 / 左右皆可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTrigger {
    Left,
    Right,
    Both,
}

/// 环末端颜色 = 各通道向 255 拉近 2/3：c → (c + 255*2) / 3。
pub fn rings_end_color_from_rgb(rgb: [u8; 3]) -> [f32; 3] {
    let map = |c: u8| (c as f32 + 255.0 * 2.0) / 3.0;
    [map(rgb[0]), map(rgb[1]), map(rgb[2])]
}

// ────────────────────────── 状态机 ──────────────────────────

/// 光标特效运行时状态。
#[derive(Debug)]
pub struct CursorEffectState {
    pub enabled: bool,
    pub color: [u8; 3],
    /// 左键点击的爆裂颜色（= 配置 `color`）。
    pub color_left: [u8; 3],
    /// 右键点击的爆裂颜色（浅红）。
    pub color_right: [u8; 3],
    /// 中键点击的爆裂颜色（浅黄）。
    pub color_middle: [u8; 3],
    pub rings_end_color: [f32; 3],
    pub scale: f32,
    pub opacity: f32,
    pub trail_speed: f32,
    pub click_speed: f32,
    pub max_trail: usize,
    pub trail_refresh_hz: u32,
    pub persistent_trail: bool,
    pub apply_curve_draw: bool,
    pub touch_mode: bool,
    pub click_trigger: ClickTrigger,
    pub middle_click_trigger: bool,
    pub hide_in_fullscreen: bool,
    pub show_on_desktop: bool,

    pub waves: Vec<Wave>,
    pub sparks: Vec<Fragment>,
    pub trail: VecDeque<TrailPoint>,
    pub scroll_rings: Vec<ScrollRing>,
    pub last_pos: Option<(f32, f32)>,
    pub is_down: bool,
    pub last_frame_time: Instant,
    pub last_trail_emit: Instant,
    /// 进程起点（字符模式用相对时间做 ~200ms 周期的闪烁节拍）。
    pub instant0: Instant,
    /// 拖尾点稳定 id 分配器。
    pub trail_serial: u64,
}

impl Default for CursorEffectState {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorEffectState {
    /// 构造并填充默认值。
    pub fn new() -> Self {
        let color = [45, 175, 255]; // 默认左键爆裂蓝
        let now = Instant::now();
        Self {
            enabled: true,
            color,
            color_left: color,
            color_right: [255, 150, 150],  // 浅红（右键）
            color_middle: [255, 235, 150], // 浅黄（中键）
            rings_end_color: rings_end_color_from_rgb(color),
            scale: 1.5,
            opacity: 1.0,
            trail_speed: 1.0,
            click_speed: 1.0,
            max_trail: 1200, // 密集 ~2px 采样；1200 点 ≈ 2400px，快速拖动不再被点容量截断
            // （旧 320 点 ≈ 640px，快拖必然打满 → 拖尾恒定长度）
            trail_refresh_hz: 40,
            persistent_trail: false,
            apply_curve_draw: false,
            touch_mode: false,
            click_trigger: ClickTrigger::Left,
            middle_click_trigger: false,
            hide_in_fullscreen: true,
            show_on_desktop: true,
            waves: Vec::with_capacity(8),
            sparks: Vec::with_capacity(32),
            trail: VecDeque::with_capacity(16),
            scroll_rings: Vec::with_capacity(4),
            last_pos: None,
            is_down: false,
            last_frame_time: now,
            last_trail_emit: now,
            instant0: now,
            trail_serial: 0,
        }
    }

    /// alpha(value) = clamp(value*opacity, 0, 1)
    #[inline]
    pub fn alpha(&self, value: f32) -> f32 {
        let v = value.clamp(0.0, 1.0) * self.opacity;
        v.clamp(0.0, 1.0)
    }

    /// 清空所有粒子并复位即时状态（关闭特效时调用）。
    pub fn clear(&mut self) {
        self.waves.clear();
        self.sparks.clear();
        self.trail.clear();
        self.scroll_rings.clear();
        self.is_down = false;
        self.last_pos = None;
        self.last_frame_time = Instant::now();
    }

    /// weightProp(t) = min(2 - |4*(t-0.5)|, 1)：环弧段两端的权重（中段 1，两端收敛）。
    #[inline]
    pub fn weight_prop(t: f32) -> f32 {
        (2.0 - (4.0 * (t - 0.5)).abs()).min(1.0)
    }

    /// 环描边色：ringsStart→给定末端色插值，t = min(1.2*r, 1)。
    /// 使用 per-wave 快照的末端色，避免被后续按键颜色覆盖。
    pub fn ring_rgb_at_with(end: [f32; 3], r_prog: f32) -> [f32; 3] {
        let t = (1.2 * r_prog).min(1.0);
        let start = [250.0, 252.0, 252.0];
        let lerp = |s: f32, e: f32| (s * (1.0 - t) + e * t).round();
        [
            lerp(start[0], end[0]),
            lerp(start[1], end[1]),
            lerp(start[2], end[2]),
        ]
    }

    /// 当前全局颜色归一化到 [0,1]，用于在创建时给实体打颜色快照。
    #[inline]
    pub fn color_norm(&self) -> [f32; 3] {
        [
            self.color[0] as f32 / 255.0,
            self.color[1] as f32 / 255.0,
            self.color[2] as f32 / 255.0,
        ]
    }

    /// ringAlpha(r) = min(1.1 - 0.3*r, 1)
    #[inline]
    pub fn ring_alpha(&self, r_prog: f32) -> f32 {
        (1.1 - 0.3 * r_prog).min(1.0)
    }
}

// ────────────────────────── 事件注入（点击 / 移动 / 滚轮）──────────────────────────

impl ClickTrigger {
    /// 是否接受此按键事件。`is_left`/`is_right`/`is_middle` 三个布尔互斥（由调用方从按钮 code
    /// 推得）。
    pub fn accepts(&self, is_left: bool, is_right: bool) -> bool {
        match self {
            ClickTrigger::Left => is_left,
            ClickTrigger::Right => is_right,
            ClickTrigger::Both => is_left || is_right,
        }
    }
}

impl CursorEffectState {
    /// 创建一次点击爆裂：一条爆裂圆盘 + 外圈环（wave）+ 数颗火花（sparks）。
    pub fn create_effects<R: EffectRng>(&mut self, x: f32, y: f32, rng: &mut R) {
        let rr = create_click_cfg::RINGS_R_ROUND_RATE_LIST;
        let pick = |rng: &mut R, slice: &[f32]| -> f32 {
            let i = (rng.f32() * slice.len() as f32) as usize;
            slice[i.min(slice.len() - 1)]
        };
        let wave_seed =
            ((rng.f32() as f64 * 1e9) as u64) ^ (((rng.f32() as f64 * 1e9) as u64) << 32);
        self.waves.push(Wave {
            x,
            y,
            r: 0.0,
            life: 0.0,
            seed: wave_seed,
            color: self.color_norm(),
            rings_end_color: self.rings_end_color,
            ring: Ring {
                ang: rng.f32() * TAU,
                rs: pick(rng, &create_click_cfg::RINGS_RS_LIST),
                segs: [
                    RingSeg {
                        off: 0.0,
                        len: create_click_cfg::RINGS_LEN,
                        r_round_rate: pick(rng, &rr),
                    },
                    RingSeg {
                        off: (rng.f32() * 3.0 - 1.5) * PI,
                        len: create_click_cfg::RINGS_LEN,
                        r_round_rate: pick(rng, &rr),
                    },
                ],
            },
        });
        let speed_adjust = self.scale / 1.5;
        for _ in 0..create_click_cfg::SPARKS_COUNT {
            let a = rng.f32() * TAU;
            let speed = (4.8 + rng.f32() * 2.0) * speed_adjust;
            self.sparks.push(Fragment {
                x,
                y,
                vx: a.cos() * speed,
                vy: a.sin() * speed,
                rot: rng.f32() * TAU,
                rs: (rng.f32() - 0.5) * 0.28,
                s: (4.0 + rng.f32() * 3.0) * self.scale,
                a: 1.0,
                f: 0.9,
                from_click: true,
                color: self.color_norm(), // 点击火花：对应按键的爆裂颜色
            });
        }
    }

    /// 指针移动：按下（或常显拖尾）时按 ~2px 间距采样拖尾点，并按 0.3 概率
    /// 从移动方向甩出一颗白色拖尾火花。
    pub fn on_move<R: EffectRng>(&mut self, nx: f32, ny: f32, _now: Instant, rng: &mut R) {
        if !self.is_down && !self.persistent_trail {
            return;
        }
        let p = (nx, ny);
        let Some(prev) = self.last_pos else {
            self.last_pos = Some(p);
            return;
        };
        self.last_pos = Some(p);

        // 拖尾采样参考点 = 已入队的最后一个点（trail 为空时退回 last_pos）。
        // 这样拖尾点沿路径保持 ~2px 间距，与指针事件采样率无关。若以"单个事件"
        // 的 last_pos 为参考，高轮询率鼠标快速晃动时单事件位移常 <2px（转向点
        // 附近速度低），导致只在高速过中点附近 push 点、转向点处断档 → 点聚集
        // 在中点而头在极值 → 拖尾退化成"锁定鼠标"的笔直直线；同时点稀疏 → 折线
        // 以长直线弦连接采样点，快速曲线运动下锯齿明显。
        let (rx, ry) = self.trail.back().map(|tp| (tp.x, tp.y)).unwrap_or(prev);
        let d = ((p.0 - rx).powi(2) + (p.1 - ry).powi(2)).sqrt();
        if d > 2.0 {
            // 对移动段按 ~2px 细分，即使单事件大跳变也生成密集折线点，保证拖尾
            // 沿真实路径平滑跟踪曲线（消除长弦带来的锯齿）。
            let steps = ((d / 2.0).ceil().max(1.0)) as usize;
            for k in 1..=steps {
                let t = k as f32 / steps as f32;
                self.trail_serial = self.trail_serial.wrapping_add(1);
                self.trail.push_back(TrailPoint {
                    x: rx + (p.0 - rx) * t,
                    y: ry + (p.1 - ry) * t,
                    life: 1.0,
                    id: self.trail_serial,
                    color: self.color_norm(),
                });
            }
            while self.trail.len() > self.max_trail {
                self.trail.pop_front();
            }
            if rng.f32() < 0.3 {
                let a = rng.f32() * TAU;
                let sa = self.scale / 1.5;
                self.sparks.push(Fragment {
                    x: p.0 + a.cos() * 10.0 * self.scale,
                    y: p.1 + a.sin() * 10.0 * self.scale,
                    vx: a.cos() * 1.3 * sa,
                    vy: a.sin() * 1.3 * sa,
                    rot: rng.f32() * TAU,
                    rs: 0.16,
                    s: 9.0 * self.scale,
                    a: 0.7,
                    f: 0.95,
                    from_click: false,
                    color: [1.0, 1.0, 1.0], // 拖尾产生的火花：白色
                });
            }
        }
        self.last_pos = Some(p);
    }
}

// ────────────────────────── 每帧推进（寿命 / 运动 / 回收）──────────────────────────

impl CursorEffectState {
    /// 是否有存活粒子；无粒子时状态机可休眠。
    pub fn has_work(&self) -> bool {
        !self.waves.is_empty()
            || !self.sparks.is_empty()
            || !self.trail.is_empty()
            || !self.scroll_rings.is_empty()
    }

    /// 推进一帧。返回本帧后是否仍有粒子存活（供调用方决定 redraw）。
    pub fn advance(&mut self, now: Instant) -> bool {
        if !self.enabled {
            self.clear();
            return false;
        }
        if !self.has_work() {
            // 静止时同步时间基准，避免首帧 delta 顶到上限。
            self.last_frame_time = now;
            return false;
        }
        let elapsed = now.saturating_duration_since(self.last_frame_time);
        let delta_ms = (elapsed.as_secs_f32() * 1000.0).min(MAX_DELTA_MS);
        self.last_frame_time = now;
        let base_scale = delta_ms / BASE_FRAME_MS;
        let trail_fs = base_scale * self.trail_speed;
        let click_fs = base_scale * self.click_speed;
        self.update_trail(trail_fs);
        self.update_waves(click_fs);
        self.update_sparks(click_fs, trail_fs);
        self.update_scroll_rings(base_scale);
        self.has_work()
    }

    /// updateTrail：拖尾点寿命衰减（绘制在 render 层）。
    fn update_trail(&mut self, trail_fs: f32) {
        let n = self.trail.len();
        // 点按“距离采样（2px）+ 按帧衰减”平衡：存活长度 ≈ 拖动速度 × 点寿命。
        // 0.020 → 寿命 ~50 帧（60Hz）：快速拖动拖尾明显更长，慢拖自然更短。
        let base_decay = if self.persistent_trail {
            0.020
        } else if self.is_down {
            0.020
        } else {
            0.10 // 松开后快速收尾（不拖拽时拖尾尽快消失）
        };
        let base_decay = base_decay * trail_fs;
        let max_step = 0.42;
        let span = (n as f32 - 1.0).max(1.0);
        let mut i = n;
        while i > 0 {
            i -= 1;
            let along = if n > 1 { i as f32 / span } else { 1.0 };
            let bias = 1.25 - 0.55 * along;
            let step = (base_decay * bias).min(max_step);
            let p = &mut self.trail[i];
            p.life -= step;
            if p.life <= 0.0 {
                self.trail.remove(i); // VecDeque::remove O(n)，max 16 可接受
            }
        }
    }

    /// updateWaves：爆裂圆盘半径增长 + 外圈环旋转 + 寿命/回收。
    /// 段几何（start/end/len/lineWidthMul）在 render 层按 ring_prog 重新计算，
    /// state 层只保留寿命与旋转；`weight_prop`/`ring_rgb_at`/`ring_alpha` 作纯函数供 render 调用。
    fn update_waves(&mut self, click_fs: f32) {
        let mut i = 0;
        while i < self.waves.len() {
            // 先在可变借用内完成全部计算，再决定回收，避免在借用中 swap_remove。
            let recycle = {
                let w = &mut self.waves[i];
                w.life += click_fs;
                let wave_prog = (w.life / filled_cfg::MAX_LIFE).min(1.0); // 驱动运动
                let ease = 1.0 - (1.0 - wave_prog).powi(3);
                w.r = filled_cfg::R_ADD_RATE * self.scale * ease;
                w.ring.ang -= w.ring.rs * click_fs;
                // 回收等淡出完成（FADE_EXTEND 延长窗口），而非运动完成就回收。
                let fill_fade_done = w.life >= filled_cfg::MAX_LIFE * filled_cfg::FADE_EXTEND;
                let ring_fade_done = w.life >= rings_cfg::MAX_LIFE * rings_cfg::FADE_EXTEND;
                fill_fade_done && ring_fade_done
            };
            if recycle {
                self.waves.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// updateSparks：火花位置积分 + 速度/自旋摩擦衰减 + 透明度寿命。
    fn update_sparks(&mut self, click_fs: f32, trail_fs: f32) {
        let mut i = 0;
        while i < self.sparks.len() {
            let recycle = {
                let s = &mut self.sparks[i];
                let fs = if s.from_click { click_fs } else { trail_fs };
                s.x += s.vx * fs;
                s.y += s.vy * fs;
                s.vx *= s.f.powf(fs);
                s.vy *= s.f.powf(fs);
                s.rot += s.rs * fs;
                // 拖尾产生的火花（from_click=false）消失更慢（减半）。
                let a_decay = if s.from_click { 0.020 } else { 0.010 };
                s.a -= a_decay * fs;
                s.a <= 0.0
            };
            if recycle {
                self.sparks.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// 滚轮滚动：在光标处产生/刷新一条代码圆环。
    ///
    /// `amount > 0` 为滚轮向下（淡橙、顺时针），`amount < 0` 为滚轮向上（淡绿、逆时针）。
    /// 旋转角按 `amount`（v120 单位，1 整档 = 120）比例注入 → 转动速度跟随滚轮速度。
    /// 同方向且位置相近时刷新已有环（保持连续、不闪跳）；否则新建一条。
    pub fn on_scroll<R: EffectRng>(&mut self, x: f32, y: f32, amount: f32, rng: &mut R) {
        if !self.enabled || amount == 0.0 {
            return;
        }
        let (dir, color) = if amount > 0.0 {
            (1.0_f32, scroll_cfg::COLOR_DOWN)
        } else {
            (-1.0_f32, scroll_cfg::COLOR_UP)
        };
        let angle_step = dir * scroll_cfg::ANGLE_PER_NOTCH * (amount.abs() / 120.0).clamp(0.0, 4.0);
        let merged = self
            .scroll_rings
            .iter_mut()
            .find(|r| r.dir == dir && (r.x - x).hypot(r.y - y) < scroll_cfg::MERGE_DIST);
        if let Some(r) = merged {
            // 滚轮速率 = 注入角 ÷ 距上一滚动事件的帧数（rad/帧）。慢滚（事件间隔
            // 长）→ 低速率 → 火花慢；快滚（事件密集）→ 高速率 → 火花快。
            let gap = (r.idle as f32).max(1.0);
            r.x = x;
            r.y = y;
            r.idle = 0;
            r.ang += angle_step;
            r.ang_rate = angle_step / gap;
        } else {
            let seed =
                ((rng.f32() as f64 * 1e9) as u64) ^ (((rng.f32() as f64 * 1e9) as u64) << 32);
            self.scroll_rings.push(ScrollRing {
                x,
                y,
                dir,
                color,
                ang: angle_step,
                last_ang: 0.0,
                idle: 0,
                ang_rate: angle_step, // 新环无历史间隔：按单帧事件折算（≈旧行为）
                seed,
                spark_sn: 0,
            });
            while self.scroll_rings.len() > scroll_cfg::MAX_RINGS {
                self.scroll_rings.remove(0);
            }
        }
    }

    /// 滚动圆环逐帧推进：旋转角由滚轮注入，此处只做停止后的"惯性"旋转；
    /// 环在可见期间始终跟随光标；随后按 GRACE_FRAMES + FADE_FRAMES 逐个淡出并回收。
    /// 转动帧会按扫过的角度把火花沿切向"甩出去"。
    fn update_scroll_rings(&mut self, base_scale: f32) {
        let mut spawned: Vec<Fragment> = Vec::new();
        let mut i = 0;
        while i < self.scroll_rings.len() {
            let recycle = {
                let r = &mut self.scroll_rings[i];
                // 可见期间跟随光标：滚动同时移动鼠标，圆环不会甩在后面。
                if let Some((cx, cy)) = self.last_pos {
                    r.x = cx;
                    r.y = cy;
                }
                r.idle = r.idle.saturating_add(1);
                if r.idle <= scroll_cfg::ROT_STOP_FRAMES {
                    let f = 1.0 - r.idle as f32 / scroll_cfg::ROT_STOP_FRAMES as f32;
                    r.ang += scroll_cfg::COAST_ANGLE * r.dir * f * base_scale;
                }
                // 本帧扫过的角 → 切向甩出火花。
                let ds = r.ang - r.last_ang;
                r.last_ang = r.ang;
                // 滚轮速率逐帧指数衰减：滚动停止后惯性阶段甩出的火花也越来越慢。
                r.ang_rate *= 0.92;
                if ds.abs() > 0.02 {
                    let rad = scroll_cfg::RADIUS * (self.scale / 1.5);
                    let n = ((ds.abs() / 0.08).round() as usize).clamp(1, 3);
                    // 火花力度 ∝ 滚轮速率（rad/帧）：慢滚 → 慢火花、快滚 → 快火花；
                    // 下限 0.02 保证停止滚动后的惯性火花仍可见（并随衰减趋零）。
                    let rate = r.ang_rate.abs().max(0.02);
                    for _ in 0..n {
                        r.spark_sn = r.spark_sn.wrapping_add(1);
                        let th = hash_unit(
                            (r.seed as usize ^ 0xDEF0) ^ (r.spark_sn as usize).wrapping_mul(7919),
                        ) * TAU;
                        let px = r.x + th.cos() * rad;
                        let py = r.y + th.sin() * rad;
                        // 切向单位向量（dir>0 顺时针）：保持 in-place 旋转方向。
                        let tx = r.dir * (-th.sin());
                        let ty = r.dir * th.cos();
                        // 力度范围化：速度乘系数 0.3..1.6 分布 + 切向角度 ±0.35rad 抖动，
                        // 让火花以不同距离和角度散开，光标周围不会空。
                        let k = hash_unit(r.spark_sn as usize ^ 0x51) * 1.3 + 0.3;
                        let speed = rad * rate * 0.6 * k;
                        let jitter = (hash_unit(r.spark_sn as usize ^ 0x52) - 0.5) * 0.7;
                        let ct = tx * jitter.cos() - ty * jitter.sin();
                        let st = tx * jitter.sin() + ty * jitter.cos();
                        // 向外（径向）分量也范围化：0.25..0.9 倍速度。
                        let outward = speed * (0.25 + hash_unit(r.spark_sn as usize ^ 0x11) * 0.65);
                        let vx = ct * speed + th.cos() * outward;
                        let vy = st * speed + th.sin() * outward;
                        spawned.push(Fragment {
                            x: px,
                            y: py,
                            vx,
                            vy,
                            rot: hash_unit(r.spark_sn as usize ^ 0x22) * TAU,
                            rs: (hash_unit(r.spark_sn as usize ^ 0x33) - 0.5) * 0.3,
                            s: (3.0 + hash_unit(r.spark_sn as usize ^ 0x44) * 2.0) * self.scale,
                            a: 0.85,
                            f: 0.9,
                            from_click: false,
                            color: r.color,
                        });
                    }
                }
                r.idle > scroll_cfg::GRACE_FRAMES + scroll_cfg::FADE_FRAMES as u16
            };
            if recycle {
                self.scroll_rings.swap_remove(i);
            } else {
                i += 1;
            }
        }
        self.sparks.extend(spawned);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// 确定性随机桩：循环返回固定序列，便于断言粒子数与坐标。
    struct SequentialRng {
        seq: Vec<f32>,
        idx: usize,
    }
    impl SequentialRng {
        fn new(seq: &[f32]) -> Self {
            Self {
                seq: seq.to_vec(),
                idx: 0,
            }
        }
    }
    impl EffectRng for SequentialRng {
        fn f32(&mut self) -> f32 {
            let v = self.seq[self.idx % self.seq.len()];
            self.idx += 1;
            v
        }
    }

    #[test]
    fn end_color_formula_is_two_thirds_blend() {
        // c=45 → (45 + 510) / 3 = 185；c=175 → (175+510)/3 = 228.33…；c=255 → 255
        let e = rings_end_color_from_rgb([45, 175, 255]);
        assert!((e[0] - 185.0).abs() < 1e-3);
        assert!((e[1] - 228.333_3).abs() < 1e-3);
        assert!((e[2] - 255.0).abs() < 1e-3);
    }

    #[test]
    fn weight_prop_is_unit_bell_peak_at_half() {
        assert!((CursorEffectState::weight_prop(0.5) - 1.0).abs() < 1e-3);
        // 端点应为 0（|4*(0-0.5)|=2，2-2=0）
        assert!((CursorEffectState::weight_prop(0.0)).abs() < 1e-3);
        assert!((CursorEffectState::weight_prop(1.0)).abs() < 1e-3);
    }

    #[test]
    fn ring_alpha_clamped_to_one() {
        let s = CursorEffectState::new();
        assert!((s.ring_alpha(0.0) - 1.0).abs() < 1e-3);
        assert!((s.ring_alpha(1.0) - 0.8).abs() < 1e-3);
    }

    #[test]
    fn create_effects_makes_one_wave_and_four_sparks() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        s.create_effects(10.0, 10.0, &mut rng);
        assert_eq!(s.waves.len(), 1);
        assert_eq!(s.sparks.len(), create_click_cfg::SPARKS_COUNT);
        assert!(s.has_work());
        // wave 初值
        let w = s.waves[0];
        assert_eq!((w.x, w.y), (10.0, 10.0));
        assert_eq!(w.r, 0.0);
        assert_eq!(w.life, 0.0);
        assert_eq!(w.ring.segs.len(), 2);
        // sparks 来自点击
        for sp in &s.sparks {
            assert!(sp.from_click);
            assert_eq!(sp.a, 1.0);
            assert_eq!(sp.f, 0.9);
        }
    }

    #[test]
    fn on_move_only_trails_when_down_or_persistent() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        let now = Instant::now();
        s.is_down = false;
        s.persistent_trail = false;
        s.on_move(0.0, 0.0, now, &mut rng);
        assert!(s.trail.is_empty());
        s.is_down = true;
        s.on_move(10.0, 10.0, now, &mut rng); // last_pos 先被设
        s.on_move(60.0, 60.0, now, &mut rng); // 距离 70>2 → 沿段 ~2px 细分采样
        assert!(!s.trail.is_empty());
        assert!(s.trail.len() <= s.max_trail);
        // 拖尾点应密集覆盖 (10,10)→(60,60) 段，末端就是当前点
        let last = s.trail.back().unwrap();
        assert!((last.x - 60.0).abs() < 1e-3 && (last.y - 60.0).abs() < 1e-3);
    }

    #[test]
    fn trail_dense_sampling_follows_fast_shake_path() {
        // 回归：快速晃动鼠标时拖尾退化成"锁定鼠标"的笔直直线。
        // 高轮询率鼠标单事件位移常 <2px（转向点附近速度低），若以单事件 last_pos
        // 为参考，转向点附近几乎不采样 → 点聚集在中点而头在极值 → 成直线。
        // 修复：参考点改为 trail.back()（已入队最后一点）+ ~2px 细分，路径上保持
        // 密集采样。
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 64]);
        let now = Instant::now();
        s.is_down = true;
        s.on_move(0.0, 0.0, now, &mut rng); // 初始化 last_pos
        s.on_move(40.0, 0.0, now, &mut rng); // 快速段建立拖尾（参考点=trail.back()）
        assert!(!s.trail.is_empty());
        let mut x = 40.0;
        let mut dir = -1.0;
        let mut final_x = x;
        for _ in 0..400 {
            x += dir;
            if x < -40.0 {
                dir = 1.0;
            }
            if x > 40.0 {
                dir = -1.0;
            }
            final_x = x;
            s.on_move(x, 0.0, now, &mut rng); // 之后每事件仅 1px（高轮询率）
        }
        assert!(!s.trail.is_empty());
        let xs: Vec<f32> = s.trail.iter().map(|p| p.x).collect();
        for w in xs.windows(2) {
            assert!(
                (w[1] - w[0]).abs() <= 3.0 + 1e-3,
                "trail gap too large: {xs:?}"
            );
        }
        // 拖尾必须跟随晃动前进，而非卡在起点 seed 成一条直线
        let back = s.trail.back().unwrap().x;
        assert!(
            (back - final_x).abs() <= 3.0 + 1e-3,
            "trail stuck at seed (straight-line bug), back={back}, final={final_x}"
        );
    }

    #[test]
    fn advance_at_60hz_keeps_tail_alive() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        let t0 = Instant::now();
        s.is_down = true;
        s.on_move(0.0, 0.0, t0, &mut rng);
        s.on_move(50.0, 50.0, t0, &mut rng);
        assert!(!s.trail.is_empty());
        let alive = s.advance(t0 + Duration::from_millis(16)); // ~1 帧 @ 60Hz
        assert!(alive);
    }

    #[test]
    fn sparks_die_by_alpha_within_enough_frames() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        s.create_effects(0.0, 0.0, &mut rng);
        // a 每帧 -0.032*fs（fs≈1 @ 60Hz×speed=1）；约 32 帧耗尽
        let mut now = Instant::now();
        for _ in 0..200 {
            now += Duration::from_millis(16);
            s.advance(now);
        }
        assert_eq!(s.sparks.len(), 0, "sparks should all decay by alpha");
    }

    #[test]
    fn waves_recycle_when_both_progress_complete() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        s.create_effects(0.0, 0.0, &mut rng);
        // ring max_life=23，filled max_life=16；运行 >23 帧应全部回收
        let mut now = Instant::now();
        for _ in 0..300 {
            now += Duration::from_millis(16);
            s.advance(now);
        }
        assert_eq!(
            s.waves.len(),
            0,
            "wave should recycle after both progress hit 1"
        );
    }

    #[test]
    fn disabled_clears_all_particles() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        s.create_effects(0.0, 0.0, &mut rng);
        assert!(!s.sparks.is_empty());
        s.enabled = false;
        let alive = s.advance(Instant::now());
        assert!(!alive);
        assert_eq!(s.sparks.len(), 0);
        assert_eq!(s.waves.len(), 0);
        assert!(s.trail.is_empty());
    }

    #[test]
    fn scroll_ring_created_direction_colored_and_merges() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        s.on_scroll(0.0, 0.0, 1.0, &mut rng);
        assert_eq!(s.scroll_rings.len(), 1);
        assert_eq!(s.scroll_rings[0].dir, 1.0);
        assert_eq!(s.scroll_rings[0].color, scroll_cfg::COLOR_DOWN);
        // 同向相近 → 复用，不新增
        s.on_scroll(2.0, 1.0, 1.0, &mut rng);
        assert_eq!(
            s.scroll_rings.len(),
            1,
            "nearby same-dir scroll reuses the ring"
        );
        // 反向 → 新建一条，方向相反、颜色不同
        s.on_scroll(0.0, 0.0, -1.0, &mut rng);
        assert_eq!(s.scroll_rings.len(), 2);
        assert_eq!(s.scroll_rings[1].dir, -1.0);
        assert_eq!(s.scroll_rings[1].color, scroll_cfg::COLOR_UP);
        // 滚动中刷新 → idle 归零，ring 仍存活
        s.scroll_rings[0].idle = 5;
        s.on_scroll(0.0, 0.0, 1.0, &mut rng);
        assert_eq!(s.scroll_rings[0].idle, 0);
    }

    #[test]
    fn scroll_ring_fades_and_recycles_after_scroll_stops() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        s.on_scroll(0.0, 0.0, 1.0, &mut rng);
        assert_eq!(s.scroll_rings.len(), 1);
        let mut now = Instant::now();
        // 模拟持续滚动 30 帧 → 一直有工作
        for _ in 0..30 {
            now += Duration::from_millis(16);
            s.on_scroll(0.0, 0.0, 1.0, &mut rng);
            assert!(s.advance(now), "active scroll keeps ring alive");
        }
        // 停止滚动 → GRACE+FADE 帧后环回收（甩出的火花稍后自然消亡）。
        let mut frame = 0u32;
        while s.scroll_rings.len() > 0 {
            now += Duration::from_millis(16);
            assert!(frame < 400, "ring should recycle within 400 frames");
            s.advance(now);
            frame += 1;
        }
        // 火花最终也会全部消失 → 无工作。
        while s.has_work() {
            now += Duration::from_millis(16);
            assert!(frame < 600, "all particles should die");
            s.advance(now);
            frame += 1;
        }
        assert!(!s.has_work(), "no work once everything fades");
    }

    #[test]
    fn scroll_ring_flings_sparks_along_rotation_direction() {
        let mut s = CursorEffectState::new();
        let mut rng = SequentialRng::new(&[0.5; 8]);
        // 下滚（整档）→ 顺时旋转 (dir=+1)：甩出的火花速度应沿切向（右手方向）。
        s.on_scroll(0.0, 0.0, 120.0, &mut rng);
        assert!(s.advance(Instant::now()));
        let rs = s.scroll_rings[0];
        let spun: Vec<&Fragment> = s
            .sparks
            .iter()
            .filter(|sp| !sp.from_click && sp.color == scroll_cfg::COLOR_DOWN)
            .collect();
        assert!(!spun.is_empty(), "rotation should fling sparks");
        for sp in spun {
            let dx = sp.x - rs.x;
            let dy = sp.y - rs.y;
            let th = dy.atan2(dx);
            let (tx, ty) = (rs.dir * (-th.sin()), rs.dir * th.cos());
            assert!(
                sp.vx * tx + sp.vy * ty > 0.0,
                "spark must fly along the ring rotation direction"
            );
        }
    }

    #[test]
    fn trail_length_scales_with_drag_speed() {
        // 回归：拖尾长度必须随拖动速度变化（点按距离采样 + 按帧衰减的平衡长度
        // ≈ 速度 × 点寿命）。旧实现 max_trail=320（≈640px）快拖必打满 → 恒定长度。
        fn trail_len_after(speed: f32, frames: usize) -> usize {
            let mut s = CursorEffectState::new();
            let mut rng = SequentialRng::new(&[0.5; 8]); // 0.5 不触发拖尾火花（<0.3 才触发）
            let mut now = Instant::now();
            s.is_down = true;
            s.on_move(0.0, 0.0, now, &mut rng); // 初始化 last_pos
            let mut x = 0.0f32;
            for _ in 0..frames {
                x += speed;
                now += Duration::from_millis(16);
                s.on_move(x, 0.0, now, &mut rng);
                s.advance(now);
            }
            s.trail.len()
        }
        let slow = trail_len_after(4.0, 300); // ~2 点/帧 → 平衡 ~100-140 点
        let fast = trail_len_after(20.0, 300); // ~10 点/帧 → 平衡 ~500-700 点
        assert!(
            fast > slow * 3,
            "fast drag must yield longer trail: fast={fast} slow={slow}"
        );
        assert!(fast <= 1200, "trail must not exceed max_trail: {fast}");
    }

    #[test]
    fn scroll_spark_speed_follows_wheel_cadence() {
        // 回归：滚轮越慢，甩出的火花必须越慢。旧实现火花速度 ∝ 每事件固定注入角
        // （ds），与事件间隔无关 → 慢滚一个档火花照样快。
        // 现改为 ∝ 滚轮速率（注入角 ÷ 距上一事件帧数）。
        fn spark_speeds_after_gap(gap_frames: u32) -> Vec<f32> {
            let mut s = CursorEffectState::new();
            let mut rng = SequentialRng::new(&[0.5; 8]);
            let mut now = Instant::now();
            s.on_scroll(0.0, 0.0, 120.0, &mut rng); // 首事件：建环
            for _ in 0..gap_frames {
                now += Duration::from_millis(16);
                s.advance(now); // 事件间隔：惯性期甩出的火花不参与比较
            }
            s.sparks.clear();
            s.on_scroll(0.0, 0.0, 120.0, &mut rng); // 间隔 gap 帧后再次滚动一整档
            now += Duration::from_millis(16);
            s.advance(now); // 本帧扫过注入角 → 甩火花
            s.sparks
                .iter()
                .filter(|sp| !sp.from_click)
                .map(|sp| sp.vx.hypot(sp.vy))
                .collect()
        }
        let fast = spark_speeds_after_gap(2); // 事件密集 → 高速率 → 快火花
        let slow = spark_speeds_after_gap(30); // 事件稀疏 → 低速率 → 慢火花
        assert!(!fast.is_empty() && !slow.is_empty());
        let min_fast = fast.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_slow = slow.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(
            min_fast > max_slow * 2.0,
            "slow scroll must fling slower sparks: fast={fast:?} slow={slow:?}"
        );
    }

    #[test]
    fn has_work_false_resyncs_time_no_particles_drift() {
        let mut s = CursorEffectState::new();
        let t1 = Instant::now() + Duration::from_millis(5000);
        let alive = s.advance(t1); // 无粒子：同步时间基准
        assert!(!alive);
        assert_eq!(s.last_frame_time, t1, "time base resynced to now");
    }

    #[test]
    fn trail_cap_respected_at_max_trail() {
        let mut s = CursorEffectState::new();
        s.max_trail = 3;
        s.is_down = true;
        let mut rng = SequentialRng::new(&[1.0; 16]); // rng=1 → 不 spawn 拖尾碎片(<0.3 失败)
        let now = Instant::now();
        for i in 0..10 {
            s.on_move(i as f32 * 50.0, i as f32 * 50.0, now, &mut rng);
        }
        assert!(s.trail.len() <= s.max_trail, "trail capped at max_trail");
    }

    #[test]
    fn click_trigger_accepts_routing() {
        assert!(ClickTrigger::Left.accepts(true, false));
        assert!(!ClickTrigger::Left.accepts(false, true));
        assert!(ClickTrigger::Right.accepts(false, true));
        assert!(!ClickTrigger::Right.accepts(true, false));
        assert!(ClickTrigger::Both.accepts(true, false));
        assert!(ClickTrigger::Both.accepts(false, true));
    }
}
