//! 光标特效"字符模式"的字形 atlas（glyph atlas）。
//!
//! 用系统默认英文字体（`sans`，经 pango/cairo 栅格化）渲染 ASCII 0x21..=0x7E
//! 共 94 个可打印字符（a-z/A-Z/数字/全部英文符号），组成一张白字黑底位图纹理。
//! fragment shader 里按字形索引取格子、采样 RGB 亮度作为掩码。
//!
//! 每个字符独立栅格化并居中在 `CELL_W x CELL_H` 的格子中（ink extents 居中），
//! 字符随其自然宽度居中——窄的 'i' 细、宽的 'W' 满格，保留真实字体形态。
//! 1px 起不适用（高分辨率），格子间以 CELL 内边距 + 居中避免串字。

use pangocairo::cairo::{Context, Format, ImageSurface};
use pangocairo::functions::{create_layout, show_layout};
use pangocairo::pango::FontDescription;

/// 格子像素宽/高（≥ 字形 ink 最大尺寸，留边距防串字）。
pub const CELL_W: usize = 64;
pub const CELL_H: usize = 64;
/// Atlas 列数 / 行数（16x8=128 格 ≥ 94 字符）。
pub const ATLAS_COLS: usize = 16;
pub const ATLAS_ROWS: usize = 8;
/// Atlas 像素宽高。
pub const ATLAS_W: usize = CELL_W * ATLAS_COLS; // 1024
pub const ATLAS_H: usize = CELL_H * ATLAS_ROWS; // 512
/// 栅格化字体像素大小。
pub const FONT_PX: f64 = 40.0;

/// 支持字符的起始/结束 ASCII（不含空格 0x20，含 ~ 0x7E）。
pub const FIRST_CHAR: u8 = 33;
pub const LAST_CHAR: u8 = 126;
/// 支持的字符个数 = 94。
pub const CHAR_COUNT: usize = (LAST_CHAR - FIRST_CHAR + 1) as usize;

/// 字符索引 -> atlas 格子列。
#[inline]
pub fn col_of(index: usize) -> usize {
    index % ATLAS_COLS
}

/// 字符索引 -> atlas 格子行。
#[inline]
pub fn row_of(index: usize) -> usize {
    index / ATLAS_COLS
}

/// ASCII -> 字符索引（在 `FIRST_CHAR..=LAST_CHAR` 区间内）。
#[inline]
pub fn index_of(ch: u8) -> usize {
    (ch - FIRST_CHAR) as usize
}

/// 用系统默认字体栅格化全部字符，展开成 BGRA（ARGB32 little-endian，预乘 alpha）
/// 位图：白字黑底。字形 RGB 亮度即掩码（边缘 AA 保留），shader 用 `max(r,g,b)`
/// 提取，不受平台字节序/swizzle 影响。
pub fn atlas_bgra() -> Vec<u8> {
    let surface = ImageSurface::create(Format::ARgb32, ATLAS_W as i32, ATLAS_H as i32)
        .expect("cairo surface");

    {
        let cr = Context::new(&surface).expect("cairo context");

        let mut font = FontDescription::from_string("sans");
        font.set_absolute_size(FONT_PX * f64::from(pango::SCALE));
        // 白字黑底：cairo 默认源色是黑色，必须显式设为白色。
        cr.set_source_rgb(1.0, 1.0, 1.0);

        for (idx, ch) in (FIRST_CHAR..=LAST_CHAR).enumerate() {
            let layout = create_layout(&cr);
            layout.context().set_round_glyph_positions(false);
            layout.set_font_description(Some(&font));
            let text = String::from(char::from(ch));
            layout.set_text(&text);

            let (ink, _logical) = layout.pixel_extents();
            let ink_w = ink.width();
            let ink_h = ink.height();

            let col = col_of(idx);
            let row = row_of(idx);
            let cell_cx = (col as i32) * CELL_W as i32 + (CELL_W / 2) as i32;
            let cell_cy = (row as i32) * CELL_H as i32 + (CELL_H / 2) as i32;
            let x = cell_cx - ink_w / 2 - ink.x();
            let y = cell_cy - ink_h / 2 - ink.y();

            cr.move_to(x.into(), y.into());
            show_layout(&cr, &layout);
        }
    }

    let data = surface.take_data().unwrap();
    data.as_ref().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_layout_matches_shader_constants() {
        assert_eq!(ATLAS_W, CELL_W * ATLAS_COLS);
        assert_eq!(ATLAS_H, CELL_H * ATLAS_ROWS);
        assert!(CHAR_COUNT <= ATLAS_COLS * ATLAS_ROWS);
    }

    #[test]
    fn atlas_bytes_are_bgra_and_sized() {
        let buf = atlas_bgra();
        assert_eq!(buf.len(), ATLAS_W * ATLAS_H * 4);
    }

    #[test]
    fn atlas_has_bright_glyph_pixels() {
        let buf = atlas_bgra();
        let bright = buf
            .chunks_exact(4)
            .filter(|px| {
                let r = px[0];
                let g = px[1];
                let b = px[2];
                u16::from(r) + u16::from(g) + u16::from(b) > 120
            })
            .count();
        assert!(bright > 1000, "expected visible glyph pixels, got {bright}");
    }

    #[test]
    fn index_mapping_roundtrips() {
        assert_eq!(index_of(b'A'), 0x41 - FIRST_CHAR as usize);
        assert_eq!(index_of(b'z'), 0x7A - FIRST_CHAR as usize);
        assert_eq!(index_of(b'~'), CHAR_COUNT - 1);
        assert_eq!(col_of(0), 0);
        assert_eq!(row_of(0), 0);
        assert_eq!(col_of(16), 0);
        assert_eq!(row_of(16), 1);
    }
}
