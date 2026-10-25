//! 拖尾条带网格与进度渐变。
//!
//! 移植自 `ba-click-fx-desktop` 的 `src/core/src/unity_trail_mesh.cpp`：
//! Unity `TrailRenderer` 的斜接（miter）条带、拐角外弧、两端半圆帽，以及
//! 按弧长进度取色的渐变。渲染层把这条网格拆成逐段的四边形绘制（见
//! `render.rs`），因此这里除了顶点外还保留 `progress`/`transverse`。

use super::curve::{linear_to_srgb, Color};
use super::sim::Point;

const MINIMUM_SEGMENT_LENGTH: f32 = 1.0e-5;
const MAXIMUM_INNER_MITER_RATIO: f32 = 8.0;
const TURN_EPSILON: f32 = 1.0e-6;
const DARK_KEY_TIME: f32 = 0.579_415_58;
const BRIGHT_KEY_TIME: f32 = 0.979_415_58;
/// Unity 用活动线性色彩空间序列化渐变关键帧。
const DARK_KEY: [f32; 3] = [0.0, 0.094_869_91, 0.282_352_95];
const BRIGHT_KEY: [f32; 3] = [0.0, 0.390_581_37, 1.0];

/// 条带网格的一个顶点：屏幕像素坐标 + 纵向进度 + 横向 0..1。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrailVertex {
    pub x: f32,
    pub y: f32,
    pub progress: f32,
    pub transverse: f32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Vec2 {
    x: f32,
    y: f32,
}

impl Vec2 {
    const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    fn add(self, other: Vec2) -> Vec2 {
        Vec2::new(self.x + other.x, self.y + other.y)
    }
    fn sub(self, other: Vec2) -> Vec2 {
        Vec2::new(self.x - other.x, self.y - other.y)
    }
    fn mul(self, scale: f32) -> Vec2 {
        Vec2::new(self.x * scale, self.y * scale)
    }
    fn cross(self, other: Vec2) -> f32 {
        self.x * other.y - self.y * other.x
    }
    fn dot(self, other: Vec2) -> f32 {
        self.x * other.x + self.y * other.y
    }
    fn length(self) -> f32 {
        self.dot(self).sqrt()
    }
    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Segment {
    valid: bool,
    from: Vec2,
    to: Vec2,
    tangent: Vec2,
    normal: Vec2,
    from_left: Vec2,
    from_right: Vec2,
    to_left: Vec2,
    to_right: Vec2,
}

#[derive(Debug, Clone, Default)]
struct Join {
    inner: Vec2,
    inner_transverse: f32,
    outer_transverse: f32,
    outer_arc: Vec<Vec2>,
}

fn make_vertex(position: Vec2, progress: f32, transverse: f32) -> TrailVertex {
    TrailVertex {
        x: position.x,
        y: position.y,
        progress,
        transverse,
    }
}

fn append_clockwise_triangle(
    output: &mut Vec<TrailVertex>,
    a: TrailVertex,
    b: TrailVertex,
    mut c: TrailVertex,
) {
    let mut b = b;
    let ab = Vec2::new(b.x - a.x, b.y - a.y);
    let ac = Vec2::new(c.x - a.x, c.y - a.y);
    if ab.cross(ac) < 0.0 {
        std::mem::swap(&mut b, &mut c);
    }
    output.push(a);
    output.push(b);
    output.push(c);
}

/// 生成 Unity TrailRenderer 形状的条带网格。
pub fn make_unity_trail_mesh(points: &[Point], width: f32) -> Vec<TrailVertex> {
    let mut mesh = Vec::new();
    if points.len() < 2 || !width.is_finite() || width <= 0.0 {
        return mesh;
    }

    let half_width = width * 0.5;
    let mut distances = vec![0.0f32; points.len()];
    let mut segments = vec![Segment::default(); points.len()];
    for index in 1..points.len() {
        let from = Vec2::new(points[index - 1].x, points[index - 1].y);
        let to = Vec2::new(points[index].x, points[index].y);
        let delta = to.sub(from);
        let segment_length = if delta.finite() { delta.length() } else { 0.0 };
        distances[index] = distances[index - 1] + segment_length;
        if segment_length <= MINIMUM_SEGMENT_LENGTH {
            continue;
        }

        let tangent = delta.mul(1.0 / segment_length);
        let normal = Vec2::new(-tangent.y, tangent.x);
        let offset = normal.mul(half_width);
        segments[index] = Segment {
            valid: true,
            from,
            to,
            tangent,
            normal,
            from_left: from.add(offset),
            from_right: from.sub(offset),
            to_left: to.add(offset),
            to_right: to.sub(offset),
        };
    }

    let total_length = distances[distances.len() - 1];
    if !total_length.is_finite() || total_length <= MINIMUM_SEGMENT_LENGTH {
        return mesh;
    }

    let mut joins: Vec<Option<Join>> = vec![None; points.len()];
    for point_index in 1..points.len().saturating_sub(1) {
        let (previous, next) = {
            let (left, right) = segments.split_at_mut(point_index + 1);
            (&mut left[point_index], &mut right[0])
        };
        if !previous.valid || !next.valid {
            continue;
        }

        let turn = previous.tangent.cross(next.tangent);
        if turn.abs() <= TURN_EPSILON {
            continue;
        }
        let direction_dot = previous.tangent.dot(next.tangent);
        let point = Vec2::new(points[point_index].x, points[point_index].y);
        let inner_sign = if turn > 0.0 { 1.0 } else { -1.0 };
        let outer_sign = -inner_sign;
        let previous_inner = point.add(previous.normal.mul(half_width * inner_sign));
        let next_inner = point.add(next.normal.mul(half_width * inner_sign));
        let inner_scale = next_inner.sub(previous_inner).cross(next.tangent) / turn;
        let inner = previous_inner.add(previous.tangent.mul(inner_scale));
        let inner_distance = inner.sub(point).length();
        let previous_projection = inner.sub(point).dot(previous.tangent);
        let next_projection = inner.sub(point).dot(next.tangent);
        let previous_length = previous.to.sub(previous.from).length();
        let next_length = next.to.sub(next.from).length();
        if !inner.finite()
            || inner_distance > half_width * MAXIMUM_INNER_MITER_RATIO
            || previous_projection < -previous_length - TURN_EPSILON
            || previous_projection > TURN_EPSILON
            || next_projection < -TURN_EPSILON
            || next_projection > next_length + TURN_EPSILON
        {
            continue;
        }

        let turn_angle = turn.atan2(direction_dot);
        let outer_start_angle = (previous.normal.y * outer_sign).atan2(previous.normal.x * outer_sign);
        let mut join = Join {
            inner,
            inner_transverse: if inner_sign > 0.0 { 1.0 } else { 0.0 },
            outer_transverse: if inner_sign > 0.0 { 0.0 } else { 1.0 },
            outer_arc: Vec::with_capacity(6),
        };
        let arc_steps = 5usize;
        for step in 0..=arc_steps {
            let angle = outer_start_angle + turn_angle * step as f32 / arc_steps as f32;
            join.outer_arc.push(Vec2::new(
                point.x + angle.cos() * half_width,
                point.y + angle.sin() * half_width,
            ));
        }

        if inner_sign > 0.0 {
            previous.to_left = inner;
            next.from_left = inner;
            previous.to_right = join.outer_arc[0];
            next.from_right = join.outer_arc[join.outer_arc.len() - 1];
        } else {
            previous.to_right = inner;
            next.from_right = inner;
            previous.to_left = join.outer_arc[0];
            next.from_left = join.outer_arc[join.outer_arc.len() - 1];
        }
        joins[point_index] = Some(join);
    }

    mesh.reserve(
        (points.len() - 1) * 6 + points.len() * 5 * 3 + 6,
    );
    for index in 1..segments.len() {
        let segment = segments[index];
        if !segment.valid {
            continue;
        }
        let from_progress = distances[index - 1] / total_length;
        let to_progress = distances[index] / total_length;
        append_clockwise_triangle(
            &mut mesh,
            make_vertex(segment.from_left, from_progress, 1.0),
            make_vertex(segment.to_right, to_progress, 0.0),
            make_vertex(segment.to_left, to_progress, 1.0),
        );
        append_clockwise_triangle(
            &mut mesh,
            make_vertex(segment.from_left, from_progress, 1.0),
            make_vertex(segment.from_right, from_progress, 0.0),
            make_vertex(segment.to_right, to_progress, 0.0),
        );
    }

    for point_index in 1..joins.len().saturating_sub(1) {
        let Some(join) = joins[point_index].as_ref() else {
            continue;
        };
        let progress = distances[point_index] / total_length;
        for arc in 1..join.outer_arc.len() {
            append_clockwise_triangle(
                &mut mesh,
                make_vertex(join.inner, progress, join.inner_transverse),
                make_vertex(join.outer_arc[arc - 1], progress, join.outer_transverse),
                make_vertex(join.outer_arc[arc], progress, join.outer_transverse),
            );
        }
    }

    let first = segments.iter().find(|segment| segment.valid);
    let last = segments.iter().rev().find(|segment| segment.valid);
    if let (Some(first), Some(last)) = (first, last) {
        let start_tip = first.from.sub(first.tangent.mul(half_width));
        append_clockwise_triangle(
            &mut mesh,
            make_vertex(first.from_left, 0.0, 1.0),
            make_vertex(first.from_right, 0.0, 0.0),
            make_vertex(start_tip, 0.0, 0.5),
        );

        let end_tip = last.to.add(last.tangent.mul(half_width));
        append_clockwise_triangle(
            &mut mesh,
            make_vertex(last.to_left, 1.0, 1.0),
            make_vertex(end_tip, 1.0, 0.5),
            make_vertex(last.to_right, 1.0, 0.0),
        );
    }
    mesh
}

/// Unity 拖尾渐变（线性域）。
pub fn evaluate_unity_trail_color(progress: f32) -> [f32; 3] {
    let value = progress.clamp(0.0, 1.0);
    if value <= DARK_KEY_TIME {
        let t = value / DARK_KEY_TIME;
        return lerp3([0.0, 0.0, 0.0], DARK_KEY, t);
    }
    if value <= BRIGHT_KEY_TIME {
        let t = (value - DARK_KEY_TIME) / (BRIGHT_KEY_TIME - DARK_KEY_TIME);
        return lerp3(DARK_KEY, BRIGHT_KEY, t);
    }
    BRIGHT_KEY
}

/// 屏幕显示用的拖尾颜色：上游在线性域相加后再编码回 sRGB，这里直接给出
/// 编码后的值（niri 的输出就是 sRGB 编码空间）。
pub fn trail_color_srgb(progress: f32) -> Color {
    let linear = evaluate_unity_trail_color(progress);
    let srgb = linear_to_srgb(linear);
    Color::new(srgb[0], srgb[1], srgb[2], 1.0)
}

/// 纵向覆盖：进度太靠尾端时淡出（与上游 WebGPU 传输曲线一致）。
pub fn trail_longitudinal_coverage(progress: f32) -> f32 {
    const FADE_START: f32 = 0.248_532;
    const FADE_END: f32 = 0.979_415_58;
    if progress <= FADE_START {
        return 0.0;
    }
    if progress >= FADE_END {
        return 1.0;
    }
    let value = (progress - FADE_START) / (FADE_END - FADE_START);
    value * value * value * (value * (value * 6.0 - 15.0) + 10.0)
}

fn lerp3(from: [f32; 3], to: [f32; 3], amount: f32) -> [f32; 3] {
    [
        from[0] + (to[0] - from[0]) * amount,
        from[1] + (to[1] - from[1]) * amount,
        from[2] + (to[2] - from[2]) * amount,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_trail_produces_two_triangles_plus_caps() {
        let points = [Point::new(0.0, 0.0), Point::new(10.0, 0.0)];
        let mesh = make_unity_trail_mesh(&points, 2.0);
        // 1 段 × 2 三角 + 首尾 2 个帽子三角 = 4 个三角
        assert_eq!(mesh.len(), 12);
        // 起点帽子在左侧，终点帽子在右侧
        assert!(mesh.iter().any(|v| (v.x + 1.0).abs() < 1e-4 && v.progress == 0.0));
        assert!(mesh.iter().any(|v| (v.x - 11.0).abs() < 1e-4 && v.progress == 1.0));
    }

    #[test]
    fn degenerate_input_is_empty() {
        assert!(make_unity_trail_mesh(&[], 2.0).is_empty());
        assert!(make_unity_trail_mesh(&[Point::new(0.0, 0.0)], 2.0).is_empty());
        let points = [Point::new(0.0, 0.0), Point::new(0.0, 0.0)];
        assert!(make_unity_trail_mesh(&points, 2.0).is_empty());
    }

    #[test]
    fn color_gradient_hits_keys() {
        let dark = evaluate_unity_trail_color(0.0);
        assert!((dark[2] - 0.0).abs() < 1e-6);
        let bright = evaluate_unity_trail_color(1.0);
        assert!((bright[2] - 1.0).abs() < 1e-6);
    }
}
