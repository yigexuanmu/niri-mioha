//! 最小 LZ4 block 解码器。
//!
//! 上游把四张参考纹理以 raw LZ4 block 编进可执行文件（没有帧头/校验和），
//! 这里做同样的解码，避免在仓库里塞 2.3 MB 的裸像素。

pub fn decompress_block(source: &[u8], expected_size: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(expected_size);
    let mut index = 0usize;
    let length = source.len();

    while index < length {
        let token = source[index];
        index += 1;

        let mut literals = (token >> 4) as usize;
        if literals == 15 {
            loop {
                if index >= length {
                    return output;
                }
                let extension = source[index];
                index += 1;
                literals += extension as usize;
                if extension != 255 {
                    break;
                }
            }
        }

        let literal_end = (index + literals).min(length);
        output.extend_from_slice(&source[index..literal_end]);
        index = literal_end;
        if index + 2 > length {
            break;
        }

        let offset = source[index] as usize | ((source[index + 1] as usize) << 8);
        index += 2;
        if offset == 0 || offset > output.len() {
            break;
        }

        let mut match_length = (token & 0x0F) as usize;
        if match_length == 15 {
            loop {
                if index >= length {
                    break;
                }
                let extension = source[index];
                index += 1;
                match_length += extension as usize;
                if extension != 255 {
                    break;
                }
            }
        }
        match_length += 4;

        let start = output.len() - offset;
        for step in 0..match_length {
            let byte = output[start + step];
            output.push(byte);
        }
    }

    debug_assert_eq!(output.len(), expected_size, "LZ4 解压长度不符");
    output
}

#[cfg(test)]
mod tests {
    use super::decompress_block;

    #[test]
    fn decodes_textures_to_expected_size() {
        // 四张纹理的解码长度来自上游 `generated_packed_fx_texture_data.inc`。
        let cases: [(&[u8], usize); 4] = [
            (include_bytes!("../ba_data/center_disk.lz4"), 512 * 512 * 4),
            (include_bytes!("../ba_data/dissolve_ring.lz4"), 256 * 128 * 4),
            (include_bytes!("../ba_data/triangle_atlas.lz4"), 256 * 128 * 4),
            (include_bytes!("../ba_data/trail.lz4"), 512 * 512 * 4),
        ];
        for (packed, size) in cases {
            let pixels = decompress_block(packed, size);
            assert_eq!(pixels.len(), size);
        }
    }
}
