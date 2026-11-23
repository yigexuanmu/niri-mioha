#!/usr/bin/env python3
"""C 方案基准比较工具：把上游 Windows 同构图参考帧与 ce_scene 输出对齐量化。

路线图 §12.4 的 C 方案要求“同分辨率、同场景、同 t 采样”的上游人工录帧。这个
脚本不生成基准帧，只负责在拿到基准帧之后一键回填路线图 §8 的 SSIM / 拖尾 /
雾比 / 盘心指标，避免再手工用 magick 逐项量。

用法：
    python3 scripts/ce_reference_compare.py \
        --reference ~/upstream/s5-0.300.png \
        --scene-dir /home/mioha/项目/.ce-out/all/s5 \
        --frame 0.300 \
        --out /home/mioha/项目/.ce-out/s5-c.json

依赖：ImageMagick 7（`magick` 命令）与 Python 3。
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import statistics
import subprocess
import tempfile

DEFAULT_BG = (5, 8, 20)


def run_magick(*args: str) -> str:
    result = subprocess.run(
        ["magick", *map(str, args)],
        capture_output=True,
        text=True,
        check=False,
    )
    return (result.stderr or result.stdout).strip()


def load_ppm(path: pathlib.Path) -> tuple[int, int, bytes]:
    data = path.read_bytes()
    parts = data.split(b"\n", 3)
    if len(parts) != 4 or parts[0] != b"P6":
        raise SystemExit(f"not a P6 PPM: {path}")
    width, height = map(int, parts[1].split())
    return width, height, parts[3]


def pixel(pixels: bytes, width: int, x: int, y: int) -> tuple[int, int, int]:
    index = (y * width + x) * 3
    return pixels[index], pixels[index + 1], pixels[index + 2]


def image_to_ppm(image: pathlib.Path, width: int, height: int, tmpdir: pathlib.Path) -> pathlib.Path:
    """把参考图（PNG/JPG）缩放到场景尺寸并转成 PPM；同尺寸时只转格式。"""
    out = tmpdir / "reference.ppm"
    run_magick(image, "-resize", f"{width}x{height}!", out)
    if not out.exists():
        raise SystemExit(f"image conversion failed: {image}")
    return out


def ssim(reference: pathlib.Path, frame: pathlib.Path) -> float:
    """返回 1 - 归一化 SSIM 差异（0 表示完全相同）。"""
    text = run_magick("compare", "-metric", "SSIM", reference, frame, "null:")
    match = re.search(r"\(([0-9.eE+-]+)\)", text)
    if not match:
        raise SystemExit(f"unexpected SSIM output: {text!r}")
    return 1.0 - float(match.group(1))


def saturated_run(pixels: bytes, width: int, height: int, y: int) -> int:
    best = run = 0
    for x in range(width):
        r, g, b = pixel(pixels, width, x, y)
        if b >= 250:
            run += 1
            best = max(best, run)
        else:
            run = 0
    return best


def center_rgb(pixels: bytes, width: int, height: int, cx: int, cy: int) -> list[float]:
    sums = [0.0, 0.0, 0.0]
    for yy in range(max(0, cy - 1), min(height, cy + 2)):
        for xx in range(max(0, cx - 1), min(width, cx + 2)):
            r, g, b = pixel(pixels, width, xx, yy)
            sums[0] += r
            sums[1] += g
            sums[2] += b
    return [round(v / 9.0, 1) for v in sums]


def halo_ratio(
    pixels: bytes,
    width: int,
    height: int,
    cx: int,
    cy: int,
    bg: tuple[int, int, int],
) -> dict[str, list[float]]:
    """从中心水平向右，找盘边（B 50%），再量 43px 外的远场通道比。"""
    center_b = pixel(pixels, width, cx, cy)[2]
    target = (center_b + bg[2]) / 2
    radius = 0
    for dx in range(1, min(200, width - cx)):
        if pixel(pixels, width, cx + dx, cy)[2] <= target:
            radius = dx
            break
    result: dict[str, list[float]] = {}
    for extra in (8, 20, 43):
        x = min(width - 1, cx + radius + extra)
        r, g, b = pixel(pixels, width, x, cy)
        rgb = [max(r - bg[0], 0), max(g - bg[1], 0), max(b - bg[2], 0)]
        scale = max(rgb[2], 1)
        result[str(extra)] = [round(v / scale, 3) for v in rgb]
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", required=True, type=pathlib.Path)
    parser.add_argument("--scene-dir", required=True, type=pathlib.Path)
    parser.add_argument("--frame", default="0.300")
    parser.add_argument("--out", type=pathlib.Path)
    parser.add_argument(
        "--bg",
        default="5,8,20",
        help="ce_scene 背景色 R,G,B（dark=5,8,20）",
    )
    parser.add_argument(
        "--center",
        default="960,540",
        help="点击点 x,y（s1 用；其他场景只算 SSIM/拖尾）",
    )
    args = parser.parse_args()

    frame = args.scene_dir / f"{args.frame}.ppm"
    if not frame.exists():
        raise SystemExit(f"frame not found: {frame}")
    width, height, pixels = load_ppm(frame)
    bg = tuple(int(v) for v in args.bg.split(","))
    cx, cy = (int(v) for v in args.center.split(","))

    with tempfile.TemporaryDirectory() as td:
        reference = image_to_ppm(args.reference, width, height, pathlib.Path(td))
        ref_pixels = load_ppm(reference)[2]
        ref_ssim = ssim(reference, frame)
        ref_w, ref_h = width, height
        ref_sat = saturated_run(ref_pixels, ref_w, ref_h, ref_h // 2)

    scene_sat = saturated_run(pixels, width, height, height // 2)
    center = center_rgb(pixels, width, height, cx, cy)
    halo = halo_ratio(pixels, width, height, cx, cy, bg)

    diff = 1.0 - ref_ssim
    report = {
        "reference": str(args.reference),
        "frame": str(frame),
        "size": [width, height],
        "ssim": round(ref_ssim, 6),
        "ssim_normalized_diff": round(diff, 6),
        "ssim_target": ">0.92",
        "ssim_pass": ref_ssim > 0.92,
        "saturated_run_reference": ref_sat,
        "saturated_run_scene": scene_sat,
        "saturated_delta_pct": round(
            0.0 if ref_sat == 0 else (scene_sat - ref_sat) / ref_sat * 100.0, 2
        ),
        "saturated_pass": (
            True if ref_sat == 0 else abs(scene_sat - ref_sat) <= ref_sat * 0.10
        ),
        "center_rgb_scene": center,
        "halo_ratio_scene": halo,
        "note": (
            "C 方案要求同分辨率/同场景/同 t；参考图尺寸不同时脚本会拉伸，"
            "SSIM 只作构图对齐后的近似值。"
        ),
    }

    print("| 指标 | 参考 | 场景 | 结论 |")
    print("|---|---|---|---|")
    print(
        f"| SSIM | — | {ref_ssim:.6f} | "
        f"{'PASS' if report['ssim_pass'] else 'FAIL'}（目标 >0.92） |"
    )
    print(
        f"| 拖尾饱和段 | {ref_sat}px | {scene_sat}px | "
        f"{report['saturated_delta_pct']:+.2f}% "
        f"{'PASS' if report['saturated_pass'] else 'FAIL'}（目标 <10%） |"
    )
    print(f"| 盘心 RGB（s1） | — | {center} | 记录值 |")
    print(f"| 雾通道比 43px（s1） | — | {halo['43']} | 记录值 |")

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"\nJSON -> {args.out}", file=__import__("sys").stderr)


if __name__ == "__main__":
    main()