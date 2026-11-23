//! L3 Niri 级 headless：构造完整 `State` + headless backend，跑 T1–T4。
//!
//! 这不是 `ce_scene` 的替代：`ce_scene` 只构造 `CursorEffectState`，永远抓不到
//! N20（winit 不推进）、N21（空转）、N22（挂载位置）。这里用 `State::new(..,
//! headless=true, ..)` 加 `Headless::add_renderer/add_output`，在真实 Niri 运行时
//! 里验证：
//!   T1 指针隐藏仍出特效；
//!   T2 点击后 `advance_cursor_effect` 要求出帧；
//!   T3 不可见时 `has_work` false 且就地清空；
//!   T4 全屏隐藏门控。
//!
//! 运行：
//!   cargo run --example ce_niri_headless

use std::time::{Duration, Instant};

use anyhow::Context as _;
use calloop::EventLoop;
use niri::cursor_effect::ba::Viewport;
use niri::cursor_effect::effect_should_render;
use niri::cursor_effect::render::collect_render_elements_for;
use niri::cursor_effect::state::TriggerButton;
use niri::niri::{PointerVisibility, State};
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Logical, Point};

fn main() -> anyhow::Result<()> {
    let config = niri_config::Config::parse_mem("").context("parsing empty config")?;
    let cursor_config = config.cursor_effect.clone();

    let event_loop: EventLoop<'static, State> =
        EventLoop::try_new().context("creating event loop")?;
    let display: Display<State> = Display::new().context("creating Wayland display")?;

    let mut state = State::new(
        config,
        event_loop.handle(),
        event_loop.get_signal(),
        display,
        true,  // headless
        false, // no Wayland socket
        false, // not a session instance
    )
    .map_err(|err| anyhow::anyhow!("State::new: {err}"))?;

    {
        let State { backend, niri } = &mut state;
        backend.headless().add_renderer().context("add renderer")?;
        backend.headless().add_output(niri, 1, (1920, 1080));
        niri.apply_cursor_effect_config(&cursor_config);

        let output = niri
            .global_space
            .outputs()
            .next()
            .context("headless output missing")?
            .clone();
        let output_name = output.name();
        let viewport = Viewport::new(1920, 1080);
        let start = Instant::now();
        niri.cursor_effect.set_time_origin(start);

        // T2：点击后推进必须要求出帧（winit/TTY 共用路径）。
        niri.cursor_effect_pointer_down(
            Point::<f64, Logical>::new(960.0, 540.0),
            start,
            TriggerButton::Left,
        );
        let advanced = niri.advance_cursor_effect(start + Duration::from_millis(16));
        assert!(advanced, "T2: 点击后 advance_cursor_effect 应返回 true");

        // T1：指针隐藏（Hidden）仍必须收集到特效元素。
        niri.pointer_visibility = PointerVisibility::Hidden;
        let elements = backend
            .headless()
            .with_primary_renderer(|renderer| {
                collect_render_elements_for(
                    renderer,
                    &niri.cursor_effect,
                    &output_name,
                    viewport,
                    start + Duration::from_millis(16),
                    1.0,
                )
            })
            .unwrap_or_default();
        assert!(
            !elements.is_empty(),
            "T1: 指针隐藏时特效元素必须 > 0（N22）"
        );

        // T3：打字/闲置隐藏（visible=false）后不再空转，并清空实例。
        niri.cursor_effect.set_visible_for(&output_name, false);
        assert!(
            !niri.cursor_effect.has_work_any(),
            "T3: 不可见时 has_work 必须 false（N21）"
        );
        assert!(
            !niri.advance_cursor_effect(start + Duration::from_millis(32)),
            "T3: 不可见时不应继续出帧"
        );
        assert!(!niri.cursor_effect.has_content(), "T3: 不可见时应就地清空");

        // T4：全屏隐藏门控。
        assert!(
            !effect_should_render(true, true, true, true, true, false, true),
            "T4: 全屏 + hide-in-fullscreen 必须跳过"
        );
        assert!(
            effect_should_render(true, false, true, false, true, false, true),
            "T4: 非全屏应正常渲染"
        );
    }

    println!(
        "L3 PASS: T1 pointer-hidden effect, T2 advance, T3 visibility/clear, T4 fullscreen gate"
    );
    Ok(())
}
