//! 光标特效的 FP16 离屏合成目标（路线图 P1）。
//!
//! 上游在 D3D11 里把整段特效画进 `R16G16B16A16_FLOAT` 的 direct/seed 目标，
//! 再做 bloom 与 transport。niri 原先每个图元直接画进 8-bit 输出，拖尾甚至只有
//! 一张 `Abgr8888` 的离屏纹理，强度 23.97 在写进去的瞬间就被截到 1.0（N7）。
//!
//! 这里先把**已经构建好**的元素按绘制顺序合成进一张 FP16 纹理，再整体贴回输出。
//! P1 阶段不做任何颜色变换，所以它只是「换个更宽的中间缓冲」，验收标准是与
//! 直接绘制的 8-bit 帧逐像素差 < 1/255；P2/P3 会把 direct/seed/bloom/transport
//! 都接在这张目标后面。

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::gles::{ffi, GlesError, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{
    Bind as _, Color32F, ContextId, Frame as _, Offscreen as _, Renderer as _, Texture as _,
};
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};

use super::render::{CursorEffectElement, Material};
use crate::render_helpers::bloom::BloomChain;
use crate::render_helpers::shaders::Shaders;

/// 复用缓存的离屏目标。
#[derive(Debug, Default)]
pub struct EffectTarget {
    cached: Option<CachedEffect>,
    /// 真 Bloom 金字塔 + transport（P3）。程序在 `Shaders` 里编译，这里惰性创建。
    bloom: Option<BloomChain>,
    /// 最近一次 transport 的输出，供 `CE_RESOLVE_PROBE` 读回单次 resolve 误差。
    last_ldr: Option<GlesTexture>,
}

#[derive(Debug)]
struct CachedEffect {
    texture: GlesTexture,
    context_id: ContextId<GlesTexture>,
    /// 是否真的拿到了 FP16；拿不到时退回 RGBA8，保证不黑屏。
    fp16: bool,
}

impl EffectTarget {
    /// 释放缓存纹理（特效完全静止时调用，避免 4K 下几十 MB 常驻）。
    pub fn release(&mut self) {
        self.cached = None;
        self.bloom = None;
        self.last_ldr = None;
    }

    /// 把一组元素合成进离屏目标并返回一个整体贴回元素。
    ///
    /// `elements` 的坐标是**输出物理坐标**；`bounds` 是这些元素几何的并集
    /// （已经裁到视口）。绘制时把每个元素的 `dst` 平移 `-bounds.loc`，于是
    /// 元素自身的 uniform / 纹理坐标完全不用改。
    #[allow(clippy::too_many_arguments)]
    pub fn composite(
        &mut self,
        renderer: &mut GlesRenderer,
        scale: f32,
        bounds: Rectangle<i32, Physical>,
        viewport: Size<i32, Physical>,
        elements: &[CursorEffectElement],
        material: Option<Material>,
    ) -> Option<CursorEffectElement> {
        if bounds.size.w <= 0 || bounds.size.h <= 0 || elements.is_empty() {
            return None;
        }

        let buffer_size = bounds.size.to_logical(1).to_buffer(1, Transform::Normal);
        let recreate = match &self.cached {
            Some(cached) => {
                cached.context_id != renderer.context_id() || cached.texture.size() != buffer_size
            }
            None => true,
        };

        if recreate {
            // 首选 RGBA16F：这是 N1/N7 能修好的前提（中途不能把 HDR 能量截到 1）。
            // smithay 的 `create_buffer` 在 GLES 后端把 `Abgr16161616f` 映射到
            // `RGBA16F/RGBA/HALF_FLOAT`；个别驱动不支持 renderable FP16，退回 8-bit。
            // `CE_FORCE_8BIT=1`：强制走 8-bit 离屏目标，用于对比「FP16 累积」
            // 与「8-bit 累积」的往返差（P1 验收的公平基线）。
            static FORCE_8BIT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let force_8bit =
                *FORCE_8BIT.get_or_init(|| std::env::var_os("CE_FORCE_8BIT").is_some());
            let fp16_texture = if force_8bit {
                None
            } else {
                renderer
                    .create_buffer(Fourcc::Abgr16161616f, buffer_size)
                    .map_err(|err| {
                        tracing::warn!(
                            "cursor-effect: RGBA16F effect target unavailable, \
                             falling back to 8-bit: {err}"
                        );
                    })
                    .ok()
            };
            let (texture, fp16) = match fp16_texture {
                Some(texture) => (texture, true),
                None => (
                    renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?,
                    false,
                ),
            };
            self.cached = Some(CachedEffect {
                texture,
                context_id: renderer.context_id(),
                fp16,
            });
        }

        let cached = self.cached.as_mut()?;
        if !cached.fp16 {
            tracing::debug!("cursor-effect: effect target is 8-bit (FP16 unavailable)");
        }

        let texture_size = cached
            .texture
            .size()
            .to_logical(1, Transform::Normal)
            .to_physical(1);
        let scale = Scale::from((scale as f64).max(f64::EPSILON));
        let texture = cached.texture.clone();
        let context_id = cached.context_id.clone();
        let mut target = match renderer.bind(&mut cached.texture) {
            Ok(target) => target,
            Err(err) => {
                tracing::warn!("cursor-effect: binding effect target failed: {err}");
                return None;
            }
        };

        let result = (|| -> Result<(), GlesError> {
            let mut frame = renderer.render(&mut target, texture_size, Transform::Normal)?;
            frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(texture_size)])?;
            for element in elements {
                let mut dst = element.geometry(scale);
                dst.loc -= bounds.loc;
                if dst.size.w <= 0 || dst.size.h <= 0 {
                    continue;
                }
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
            let _ = frame.finish()?;
            Ok(())
        })();

        if let Err(err) = result {
            tracing::warn!("cursor-effect: offscreen effect pass failed: {err:?}");
            return None;
        }
        drop(target);

        // `CE_NOBLOOM=1`：跳过 Bloom/transport，直接把 direct 贴回。用于量
        // “碎片本体色相”等需要排除 Bloom 白核的诊断；生产默认关闭。
        if std::env::var_os("CE_NOBLOOM").is_some() {
            self.last_ldr = Some(texture.clone());
            return Some(CursorEffectElement::from_composite(
                texture, context_id, bounds, material,
            ));
        }

        // P3：对整帧 direct 做 BrightPass + 金字塔 + BackgroundAware transport。
        // 程序不可用时退回 direct 贴回（P1 行为），至少画面不黑。
        let programs = renderer
            .egl_context()
            .user_data()
            .get::<Shaders>()
            .and_then(|shaders| shaders.bloom.clone());
        if let Some(programs) = programs {
            if self
                .bloom
                .as_ref()
                .is_none_or(|bloom| bloom.context_id() != &renderer.context_id())
            {
                self.bloom = Some(BloomChain::new(renderer, programs));
            }
            if let Some(bloom) = self.bloom.as_mut() {
                if let Some(ldr) = bloom.render(renderer, &texture, viewport) {
                    self.last_ldr = Some(ldr.clone());
                    return Some(CursorEffectElement::from_composite(
                        ldr, context_id, bounds, material,
                    ));
                }
            }
        } else {
            tracing::warn!("cursor-effect: bloom shaders unavailable, using direct output");
        }

        self.last_ldr = Some(texture.clone());
        Some(CursorEffectElement::from_composite(
            texture, context_id, bounds, material,
        ))
    }

    /// 读回最近一次 transport 输出的左上角像素。
    ///
    /// 这是 P1 的「单次 resolve」实测：FP16 全程累积后只做一次 8-bit 输出，
    /// 与 CPU 计算的 `LinearToSrgb` 期望比较，误差应 ≤1/255。
    pub fn probe_center(&self, renderer: &mut GlesRenderer) -> Option<[u8; 4]> {
        let ldr = self.last_ldr.as_ref()?;
        let mut pixel = [0u8; 4];
        renderer
            .with_context(|gl| unsafe {
                let mut fbo = 0;
                gl.GenFramebuffers(1, &mut fbo as *mut _);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
                gl.FramebufferTexture2D(
                    ffi::FRAMEBUFFER,
                    ffi::COLOR_ATTACHMENT0,
                    ffi::TEXTURE_2D,
                    ldr.tex_id(),
                    0,
                );
                gl.ReadPixels(
                    0,
                    0,
                    1,
                    1,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    pixel.as_mut_ptr().cast(),
                );
                gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                gl.DeleteFramebuffers(1, &fbo as *const _);
            })
            .ok()?;
        Some(pixel)
    }
}
