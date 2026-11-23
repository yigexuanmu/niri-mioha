//! 最小 LZ4 block 解码器。
//!
//! 上游把四张参考纹理以 raw LZ4 block 编进可执行文件（没有帧头/校验和），
//! 这里做同样的解码，避免在仓库里塞 2.3 MB 的裸像素。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lz4Error {
    pub message: &'static str,
}

/// 解码 raw LZ4 block。
///
/// 任何截断/越界输入都必须显式失败：旧的实现在 4 处静默 `return` / `break`，
/// 然后让 `debug_assert_eq!`（release 下被编译掉）兜底，结果是纹理全黑或花屏
/// 且一条日志都没有（N25）。
pub fn decompress_block(source: &[u8], expected_size: usize) -> Result<Vec<u8>, Lz4Error> {
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
                    return Err(Lz4Error {
                        message: "literal length extension is truncated",
                    });
                }
                let extension = source[index];
                index += 1;
                literals += extension as usize;
                if extension != 255 {
                    break;
                }
            }
        }

        if index + literals > length {
            return Err(Lz4Error {
                message: "literal run exceeds input",
            });
        }
        output.extend_from_slice(&source[index..index + literals]);
        index += literals;
        if index + 2 > length {
            // 最后一个序列可以只有 literals；只要总长度对得上就接受。
            break;
        }

        let offset = source[index] as usize | ((source[index + 1] as usize) << 8);
        index += 2;
        if offset == 0 || offset > output.len() {
            return Err(Lz4Error {
                message: "invalid match offset",
            });
        }

        let mut match_length = (token & 0x0F) as usize;
        if match_length == 15 {
            loop {
                if index >= length {
                    return Err(Lz4Error {
                        message: "match length extension is truncated",
                    });
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

    if output.len() != expected_size {
        return Err(Lz4Error {
            message: "decompressed size does not match expected size",
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::decompress_block;

    #[test]
    fn truncated_input_is_rejected() {
        let mut bad = include_bytes!("../ba_data/trail.lz4").to_vec();
        bad.truncate(bad.len() / 2);
        assert!(
            decompress_block(&bad, 512 * 512 * 4).is_err(),
            "截断输入必须显式失败，不能静默返回短缓冲"
        );
        assert!(decompress_block(b"", 4).is_err(), "空输入不能当成合法纹理");
    }

    #[test]
    fn decodes_textures_to_expected_size() {
        // 四张纹理的解码长度来自上游 `generated_packed_fx_texture_data.inc`。
        let cases: [(&[u8], usize); 4] = [
            (include_bytes!("../ba_data/center_disk.lz4"), 512 * 512 * 4),
            (
                include_bytes!("../ba_data/dissolve_ring.lz4"),
                256 * 128 * 4,
            ),
            (
                include_bytes!("../ba_data/triangle_atlas.lz4"),
                256 * 128 * 4,
            ),
            (include_bytes!("../ba_data/trail.lz4"), 512 * 512 * 4),
        ];
        for (packed, size) in cases {
            let pixels = decompress_block(packed, size).expect("内置纹理必须可解码");
            assert_eq!(pixels.len(), size);
        }
    }
}
