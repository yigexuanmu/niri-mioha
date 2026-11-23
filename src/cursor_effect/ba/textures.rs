//! 四张参考纹理的资源与惰性解码。
//!
//! 许可证：GPL-2.0-only（派生自 CialloKing/ba-click-fx-desktop）。
//! 见 THIRD-PARTY-NOTICES.txt 与 LICENSES/GPL-2.0-only.txt。
//!
//! 数据直接来自上游 `ba-click-fx-desktop` 内嵌的 LZ4 块（同一份字节，
//! SHA256 已在提取时校验）。解码后是紧凑的 RGBA8，供 GLES 上传。

use std::sync::OnceLock;

use super::lz4::decompress_block;

/// 一张已解码的 RGBA8 纹理。
pub struct FxTexture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 中心圆盘：径向软边圆（R 通道当遮罩，RGB 带细节）。
pub fn center_disk() -> &'static FxTexture {
    static CELL: OnceLock<FxTexture> = OnceLock::new();
    CELL.get_or_init(|| FxTexture {
        width: 512,
        height: 512,
        rgba: decompress_block(include_bytes!("../ba_data/center_disk.lz4"), 512 * 512 * 4)
            .expect("center_disk.lz4 必须可解码"),
    })
}

/// 溶解环：横向渐变的条带纹理（沿环周向采样，径向走 v）。
pub fn dissolve_ring() -> &'static FxTexture {
    static CELL: OnceLock<FxTexture> = OnceLock::new();
    CELL.get_or_init(|| FxTexture {
        width: 256,
        height: 128,
        rgba: decompress_block(
            include_bytes!("../ba_data/dissolve_ring.lz4"),
            256 * 128 * 4,
        )
        .expect("dissolve_ring.lz4 必须可解码"),
    })
}

/// 三角碎片图集：左右两帧，各占 u 的半个宽度。
pub fn triangle_atlas() -> &'static FxTexture {
    static CELL: OnceLock<FxTexture> = OnceLock::new();
    CELL.get_or_init(|| FxTexture {
        width: 256,
        height: 128,
        rgba: decompress_block(
            include_bytes!("../ba_data/triangle_atlas.lz4"),
            256 * 128 * 4,
        )
        .expect("triangle_atlas.lz4 必须可解码"),
    })
}

/// 拖尾：沿条带纵向（progress）变化的纹理。
pub fn trail() -> &'static FxTexture {
    static CELL: OnceLock<FxTexture> = OnceLock::new();
    CELL.get_or_init(|| FxTexture {
        width: 512,
        height: 512,
        rgba: decompress_block(include_bytes!("../ba_data/trail.lz4"), 512 * 512 * 4)
            .expect("trail.lz4 必须可解码"),
    })
}
