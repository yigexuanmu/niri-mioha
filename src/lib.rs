// 与上游 C++/GLSL 常量逐位对齐，保留超出 f32 精度的十进制尾数是有意的。
#![allow(clippy::excessive_precision)]
// 渲染/布局函数天然携带大量几何与配置参数，拆结构体会让调用点更不可读。
#![allow(clippy::too_many_arguments)]
// 既有上游代码的类型别名风格。
#![allow(clippy::type_complexity)]
// `Output` 等 Wayland 句柄作为 map key 是既有架构（P6 沿用）。
#![allow(clippy::mutable_key_type)]

#[macro_use]
extern crate tracing;

#[cfg(feature = "dbus")]
pub mod a11y;
#[cfg(feature = "agent")]
pub mod agent;
pub mod animation;
pub mod backend;
pub mod cli;
pub mod cursor;
pub mod cursor_effect;
#[cfg(feature = "dbus")]
pub mod dbus;
pub mod frame_clock;
pub mod handlers;
pub mod input;
pub mod ipc;
pub mod layer;
pub mod layout;
pub mod niri;
pub mod protocols;
pub mod render_helpers;
pub mod rubber_band;
#[cfg(feature = "xdp-gnome-screencast")]
pub mod screencasting;
pub mod ui;
pub mod utils;
pub mod window;

#[cfg(test)]
mod tests;
