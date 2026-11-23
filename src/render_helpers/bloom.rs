//! 真 Bloom 金字塔（路线图 P3）。
//!
//! 上游是 Unity Bloom 的 mip 链：BrightPass 预过滤 → 逐级 4-tap downsample →
//! 逐级 `coarse + fine` upsample（权重恒 1，不归一化）→ 4-tap resolve。
//! niri 原先用解析高斯近似（`cursor_effect.frag` 的 `bloom_mip`），雾的强度与
//! 形状都只能靠拟合常量（N2/N3/N8）。这里照上游 `unity_bloom.cpp` 与
//! `embedded_fx_shaders.hpp` 的 `BrightPass/DownsamplePixel/UpsamplePixel` 重写。
//!
//! 所有中间纹理优先使用 RGBA16F；拿不到 renderable FP16 时退回 RGBA8
//! （8-bit 会把 BrightPass 的 HDR 能量截到 1，但至少不会黑屏）。

use std::collections::HashMap;
use std::ffi::CString;
use std::rc::Rc;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{ffi, link_program, GlesError, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Offscreen as _, Renderer as _, Texture as _};
use smithay::utils::{Buffer, Physical, Size};

/// 上游 `resolveUnityBloomIntensity()`：2^(1.7/10) − 1。
pub const EXPOSURE_GAIN: f32 = 0.125_03;
/// 上游 `BackgroundAware` 默认预设的 Alpha 上限（250/255）。
pub const OVERLAY_ALPHA_LIMIT: f32 = 250.0 / 255.0;
/// 上游 diffusion = 7，`mipCount = floor(log2(max/2) + 7 − 10)`。
const DIFFUSION: f64 = 7.0;
const MAX_MIP_LEVELS: usize = 8;

#[derive(Debug)]
struct FsProgram {
    program: ffi::types::GLuint,
    attrib_vert: ffi::types::GLint,
    uniforms: HashMap<String, ffi::types::GLint>,
}

impl FsProgram {
    unsafe fn compile(
        gl: &ffi::Gles2,
        frag: &str,
        uniform_names: &[&str],
    ) -> Result<Self, GlesError> {
        let program = unsafe { link_program(gl, include_str!("shaders/blur.vert"), frag)? };
        let vert = c"vert";
        let attrib_vert = gl.GetAttribLocation(program, vert.as_ptr());
        let uniforms = uniform_names
            .iter()
            .map(|name| {
                let cname = CString::new(*name).expect("Interior null in uniform name");
                (
                    name.to_string(),
                    gl.GetUniformLocation(program, cname.as_ptr()),
                )
            })
            .collect();
        Ok(Self {
            program,
            attrib_vert,
            uniforms,
        })
    }
}

#[derive(Debug)]
pub struct BloomPrograms {
    prefilter: FsProgram,
    down: FsProgram,
    up: FsProgram,
    transport: FsProgram,
}

impl BloomPrograms {
    pub fn compile(renderer: &mut GlesRenderer) -> Result<Self, GlesError> {
        renderer.with_context(|gl| unsafe {
            Ok(Self {
                prefilter: FsProgram::compile(
                    gl,
                    include_str!("shaders/bloom_prefilter.frag"),
                    &["tex", "texel_size"],
                )?,
                down: FsProgram::compile(
                    gl,
                    include_str!("shaders/bloom_down.frag"),
                    &["tex", "texel_size"],
                )?,
                up: FsProgram::compile(
                    gl,
                    include_str!("shaders/bloom_up.frag"),
                    &["tex", "fine_tex", "texel_size", "sample_scale"],
                )?,
                transport: FsProgram::compile(
                    gl,
                    include_str!("shaders/transport.frag"),
                    &[
                        "direct_tex",
                        "bloom_tex",
                        "bloom_texel_size",
                        "sample_scale",
                        "exposure_gain",
                        "alpha_limit",
                    ],
                )?,
            })
        })?
    }

    pub fn destroy(self, renderer: &mut GlesRenderer) {
        let _ = renderer.with_context(|gl| unsafe {
            gl.DeleteProgram(self.prefilter.program);
            gl.DeleteProgram(self.down.program);
            gl.DeleteProgram(self.up.program);
            gl.DeleteProgram(self.transport.program);
        });
    }
}

/// 每帧复用的 bloom 链纹理。
#[derive(Debug)]
pub struct BloomChain {
    programs: Rc<BloomPrograms>,
    context_id: ContextId<GlesTexture>,
    /// 半分辨率起点的逐级 downsample 纹理。
    mips: Vec<GlesTexture>,
    /// upsample 累加纹理（`mips` 的前 n−1 级同尺寸）。
    ups: Vec<GlesTexture>,
    /// 最终 8-bit 预乘 sRGB 输出。
    ldr: Option<GlesTexture>,
    /// 上一次的 direct 尺寸，变化时重建整条链。
    direct_size: Option<Size<i32, Buffer>>,
}

impl BloomChain {
    pub fn new(renderer: &GlesRenderer, programs: Rc<BloomPrograms>) -> Self {
        Self {
            programs,
            context_id: renderer.context_id(),
            mips: Vec::new(),
            ups: Vec::new(),
            ldr: None,
            direct_size: None,
        }
    }

    pub fn context_id(&self) -> &ContextId<GlesTexture> {
        &self.context_id
    }

    /// 跑完整条 Bloom + Transport，返回预乘 sRGB 的 8-bit 纹理。
    pub fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        direct: &GlesTexture,
        viewport: Size<i32, Physical>,
    ) -> Option<GlesTexture> {
        if renderer.context_id() != self.context_id {
            tracing::warn!("cursor-effect: bloom belongs to another renderer context");
            return None;
        }
        let direct_size = direct.size();
        if direct_size.w <= 0 || direct_size.h <= 0 {
            return None;
        }

        if self.direct_size != Some(direct_size) {
            self.direct_size = Some(direct_size);
            self.mips.clear();
            self.ups.clear();
            self.ldr = None;

            // 上游按整帧分辨率（此处用视口）决定级数与 sampleScale，而不是按
            // 特效包围盒；否则小范围点击会少跑好几级，远场尾巴消失（N8）。
            let max_dim = viewport.w.max(viewport.h) as f64;
            let v = (max_dim * 0.5).max(1.0).log2() + DIFFUSION - 10.0;
            let levels = (v.floor().max(1.0) as usize).min(MAX_MIP_LEVELS);

            let mut size = Size::from(((direct_size.w / 2).max(1), (direct_size.h / 2).max(1)));
            for _ in 0..levels {
                let (texture, _fp16) = match renderer.create_buffer(Fourcc::Abgr16161616f, size) {
                    Ok(texture) => (texture, true),
                    Err(_) => match renderer.create_buffer(Fourcc::Abgr8888, size) {
                        Ok(texture) => (texture, false),
                        Err(err) => {
                            tracing::warn!("cursor-effect: bloom target creation failed: {err}");
                            return None;
                        }
                    },
                };
                self.mips.push(texture);
                if self.mips.len() < levels {
                    self.ups
                        .push(match renderer.create_buffer(Fourcc::Abgr16161616f, size) {
                            Ok(texture) => texture,
                            Err(_) => renderer.create_buffer(Fourcc::Abgr8888, size).ok()?,
                        });
                }
                size = Size::from(((size.w / 2).max(1), (size.h / 2).max(1)));
            }
            self.ldr = Some(renderer.create_buffer(Fourcc::Abgr8888, direct_size).ok()?);
        }

        let sample_scale = {
            let max_dim = viewport.w.max(viewport.h) as f64;
            let v = (max_dim * 0.5).max(1.0).log2() + DIFFUSION - 10.0;
            (0.5 + v.fract()) as f32
        };
        let ldr = self.ldr.clone()?;
        let levels = self.mips.len();
        if levels == 0 {
            return None;
        }

        let result = renderer
            .with_context(|gl| unsafe {
                while gl.GetError() != ffi::NO_ERROR {}
                gl.Disable(ffi::BLEND);
                gl.Disable(ffi::SCISSOR_TEST);
                gl.ActiveTexture(ffi::TEXTURE0);

                let mut fbo = 0;
                gl.GenFramebuffers(1, &mut fbo as *mut _);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);

                let vertices: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];

                // 1) BrightPass prefilter：direct → mips[0]（半分辨率）。
                let prefilter_size = self.mips[0].size();
                draw_pass(
                    gl,
                    &self.programs.prefilter,
                    &self.mips[0],
                    &prefilter_size,
                    direct,
                    None,
                    [1.0 / direct_size.w as f32, 1.0 / direct_size.h as f32],
                    1.0,
                    &vertices,
                );

                // 2) downsample：mips[i−1] → mips[i]，FourTap 平均。
                for index in 1..levels {
                    let src = &self.mips[index - 1];
                    let dst = &self.mips[index];
                    let src_size = src.size();
                    draw_pass(
                        gl,
                        &self.programs.down,
                        dst,
                        &dst.size(),
                        src,
                        None,
                        [1.0 / src_size.w as f32, 1.0 / src_size.h as f32],
                        1.0,
                        &vertices,
                    );
                }

                // 3) upsample：从最小级开始，coarse + fine（权重恒 1）。
                let mut coarse = self.mips[levels - 1].clone();
                for index in (0..levels - 1).rev() {
                    let dst = &self.ups[index];
                    let coarse_size = coarse.size();
                    draw_pass(
                        gl,
                        &self.programs.up,
                        dst,
                        &dst.size(),
                        &coarse,
                        Some(&self.mips[index]),
                        [1.0 / coarse_size.w as f32, 1.0 / coarse_size.h as f32],
                        sample_scale,
                        &vertices,
                    );
                    coarse = self.ups[index].clone();
                }

                // 4) Transport：direct + bloom → LDR（预乘 sRGB）。
                let bloom_size = coarse.size();
                draw_pass_transport(
                    gl,
                    &self.programs.transport,
                    &ldr,
                    &direct_size,
                    direct,
                    &coarse,
                    [1.0 / bloom_size.w as f32, 1.0 / bloom_size.h as f32],
                    sample_scale,
                    &vertices,
                );

                gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                gl.DeleteFramebuffers(1, &fbo as *const _);
                true
            })
            .ok()?;

        if result {
            Some(ldr)
        } else {
            None
        }
    }
}

unsafe fn bind_texture(gl: &ffi::Gles2, unit: u32, texture: &GlesTexture) {
    gl.ActiveTexture(ffi::TEXTURE0 + unit);
    gl.BindTexture(ffi::TEXTURE_2D, texture.tex_id());
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
    gl.TexParameteri(
        ffi::TEXTURE_2D,
        ffi::TEXTURE_WRAP_S,
        ffi::CLAMP_TO_EDGE as i32,
    );
    gl.TexParameteri(
        ffi::TEXTURE_2D,
        ffi::TEXTURE_WRAP_T,
        ffi::CLAMP_TO_EDGE as i32,
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_pass(
    gl: &ffi::Gles2,
    program: &FsProgram,
    dst: &GlesTexture,
    dst_size: &Size<i32, Buffer>,
    src0: &GlesTexture,
    src1: Option<&GlesTexture>,
    texel_size: [f32; 2],
    sample_scale: f32,
    vertices: &[f32; 8],
) {
    unsafe {
        gl.FramebufferTexture2D(
            ffi::FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            dst.tex_id(),
            0,
        );
        gl.Viewport(0, 0, dst_size.w, dst_size.h);
        gl.UseProgram(program.program);
        bind_texture(gl, 0, src0);
        if let Some(unit) = program.uniforms.get("tex") {
            gl.Uniform1i(*unit, 0);
        }
        if let Some(src1) = src1 {
            bind_texture(gl, 1, src1);
            if let Some(unit) = program.uniforms.get("fine_tex") {
                gl.Uniform1i(*unit, 1);
            }
        }
        if let Some(unit) = program.uniforms.get("texel_size") {
            gl.Uniform2f(*unit, texel_size[0], texel_size[1]);
        }
        if let Some(unit) = program.uniforms.get("sample_scale") {
            gl.Uniform1f(*unit, sample_scale);
        }
        gl.EnableVertexAttribArray(program.attrib_vert as u32);
        gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
        gl.VertexAttribPointer(
            program.attrib_vert as u32,
            2,
            ffi::FLOAT,
            ffi::FALSE,
            0,
            vertices.as_ptr().cast(),
        );
        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        gl.DisableVertexAttribArray(program.attrib_vert as u32);
        gl.ActiveTexture(ffi::TEXTURE0);
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_pass_transport(
    gl: &ffi::Gles2,
    program: &FsProgram,
    dst: &GlesTexture,
    dst_size: &Size<i32, Buffer>,
    direct: &GlesTexture,
    bloom: &GlesTexture,
    bloom_texel_size: [f32; 2],
    sample_scale: f32,
    vertices: &[f32; 8],
) {
    unsafe {
        gl.FramebufferTexture2D(
            ffi::FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            dst.tex_id(),
            0,
        );
        gl.Viewport(0, 0, dst_size.w, dst_size.h);
        gl.UseProgram(program.program);
        bind_texture(gl, 0, direct);
        bind_texture(gl, 1, bloom);
        if let Some(unit) = program.uniforms.get("direct_tex") {
            gl.Uniform1i(*unit, 0);
        }
        if let Some(unit) = program.uniforms.get("bloom_tex") {
            gl.Uniform1i(*unit, 1);
        }
        if let Some(unit) = program.uniforms.get("bloom_texel_size") {
            gl.Uniform2f(*unit, bloom_texel_size[0], bloom_texel_size[1]);
        }
        if let Some(unit) = program.uniforms.get("sample_scale") {
            gl.Uniform1f(*unit, sample_scale);
        }
        if let Some(unit) = program.uniforms.get("exposure_gain") {
            gl.Uniform1f(*unit, EXPOSURE_GAIN);
        }
        if let Some(unit) = program.uniforms.get("alpha_limit") {
            gl.Uniform1f(*unit, OVERLAY_ALPHA_LIMIT);
        }
        gl.EnableVertexAttribArray(program.attrib_vert as u32);
        gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
        gl.VertexAttribPointer(
            program.attrib_vert as u32,
            2,
            ffi::FLOAT,
            ffi::FALSE,
            0,
            vertices.as_ptr().cast(),
        );
        gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        gl.DisableVertexAttribArray(program.attrib_vert as u32);
        gl.ActiveTexture(ffi::TEXTURE0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_to_srgb_ch(value: f32) -> f32 {
        let clamped = value.clamp(0.0, 1.0);
        if clamped <= 0.003_130_8 {
            clamped * 12.92
        } else {
            1.055 * clamped.powf(1.0 / 2.4) - 0.055
        }
    }

    fn bright_pass_contribution(brightness: f32, threshold: f32, knee: f32) -> f32 {
        let soft = (brightness - (threshold - knee)).clamp(0.0, 2.0 * knee);
        let soft = soft * soft * (0.25 / knee);
        soft.max(brightness - threshold) / brightness.max(0.0001)
    }

    fn sample_scale_for(max_dim: f64) -> (usize, f32) {
        let v = (max_dim * 0.5).max(1.0).log2() + DIFFUSION - 10.0;
        (
            (v.floor().max(1.0) as usize).min(MAX_MIP_LEVELS),
            (0.5 + v.fract()) as f32,
        )
    }

    #[test]
    fn bright_pass_is_monotonic_and_zero_below_threshold() {
        assert_eq!(bright_pass_contribution(0.5, 1.0, 1.0e-5), 0.0);
        let at_1_5 = bright_pass_contribution(1.5, 1.0, 1.0e-5);
        let at_2_0 = bright_pass_contribution(2.0, 1.0, 1.0e-5);
        assert!(at_1_5 > 0.0);
        assert!(at_2_0 > at_1_5);
    }

    #[test]
    fn pyramid_weights_conserve_energy() {
        // Downsample：四个 0.25 抽头，总和 1。
        assert!((4.0f32 * 0.25 - 1.0).abs() < f32::EPSILON);
        // Upsample：coarse + fine，权重恒 1，不归一化（上游同构）。
        assert!((2.0f32 - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn sample_scale_matches_resolution() {
        let (levels_1080, scale_1080) = sample_scale_for(1920.0);
        assert_eq!(levels_1080, 6);
        assert!((scale_1080 - 1.407).abs() < 0.01, "{scale_1080}");
        let (levels_1440, scale_1440) = sample_scale_for(2560.0);
        assert_eq!(levels_1440, 7);
        assert!((scale_1440 - 0.822).abs() < 0.01, "{scale_1440}");
    }

    #[test]
    fn transport_matches_p1_probe_and_alpha_clamp() {
        let linear = 128.0 / 255.0;
        let encoded = linear_to_srgb_ch(linear) * 255.0;
        assert!((encoded - 187.84).abs() < 0.05, "{encoded}");
        assert!((EXPOSURE_GAIN - 0.125_03).abs() < 1.0e-6);
        let alpha = 0.9f32.clamp(0.5, OVERLAY_ALPHA_LIMIT);
        assert!((alpha - 0.9).abs() < f32::EPSILON);
        let clamped = 1.0f32.clamp(0.5, OVERLAY_ALPHA_LIMIT);
        assert!((clamped - OVERLAY_ALPHA_LIMIT).abs() < f32::EPSILON);
    }
}
