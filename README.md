# niri-mioha

个人自用的 niri 分支：**SHORiN niri + 官方 niri 最新代码**，外加一套自研的**光标粒子特效**。

> ⚠️ 个人向分支，不是发行版。功能以我自己的使用习惯为准，随时可能改动。
> 想要稳定干净的 niri，请用 [niri-wm/niri](https://github.com/niri-wm/niri)。

## 这个仓库是什么

本仓库是**从 SHORiN niri 的最新 main 重新起底**建立的，并以此为地基，增加了很多实验性功能：

```
niri-wm/niri  (官方)          ← 持续 merge
      ↓
SHORiN-KiWATA/niri (上游分支)  ← 基座 + 持续 merge
      ↓
yigexuanmu/niri-mioha         ← 本仓库，只加光标特效与按键行为调整
```

- **基座**：`SHORiN-KiWATA/niri` 的 main，保留其全部功能（网格预览 Grid Overview、放大镜、
  最小化高亮、概览缩放等）。
- **官方代码**：`niri-wm/niri` 的最新 main 也已合入，之后随上游继续 merge。
- **保留本分支独有功能**：光标点击特效，实现参考 [ba-click-fx-desktop](https://github.com/CialloKing/ba-click-fx-desktop)，
  全部用 GLES 在合成器内绘制，不依赖外部程序。

## 相对上游的两处本地改动

### 1. 光标点击特效（本分支主要亮点）

参考 ba-click-fx-desktop 的 BA 点击特效：点击时出现**蓝色圆盘 + 溶解环 + 三角碎片**，按住拖动留下**光带拖尾**，
每个元素都带一层解析近似的 Bloom 光晕。所有形状是矢量绘制，不再依赖字体栅格化。

效果要素：

- **圆盘**：点击位置的实心蔚蓝盘，生命周期内自发光向边缘渐亮，随后淡出。
- **溶解环**：从盘边扩出的细白环，随时间溶解成断点和新月弧；内侧有一层柔光，盘退场后仍可见。
- **三角碎片**：从点击点向外飞溅的青色三角形，颜色随生命期由白转青再淡出。
- **拖尾**：按住鼠标拖动产生的光带，带近场亮边与远场残尾，颜色沿笔画方向渐变。
- **光晕**：整帧 FP16 direct/seed + BrightPass + mip 金字塔的真 Bloom（与上游 Unity Bloom 同构），不再使用解析高斯近似。

#### 配置

在 niri 配置里添加 `cursor-effect { ... }` 块。所有字段可选，缺省取默认值；
长度单位是 **1080p 参考像素**，时间单位是毫秒。

```kdl
cursor-effect {
    enabled true                      // 总开关，也可用 toggle-cursor-effect 动作运行时切换
    scale 1.0                         // 整体缩放，1.0 = 上游原尺寸
    opacity 1.0                       // 整体不透明度
    tint "255,255,255"                // 每通道颜色倍率：R,G,B / #rrggbb / rgb(...)，白色保持原色

    use-linked-animation-speed true   // true：拖尾与点击共用 effect-speed
    effect-speed 1.0
    trail-speed 1.0                   // use-linked 为 false 时生效
    click-speed 1.0                   // use-linked 为 false 时生效
    trail-refresh-rate 0              // 拖尾采样频率 Hz，0 = 每次运动事件都采样（上游默认）
    enable-always-trail false         // 不按键也常显拖尾
    apply-curve-draw false            // 拖尾用 Catmull-Rom 曲线而非直线段

    click-trigger "both"              // left / right / both（上游默认左+右）
    enable-middle-click-trigger false // 中键是否触发（上游默认不触发）
    // trigger-buttons "left" "middle" // 显式触发键列表，覆盖上面两行；支持
                                      // left/right/middle/back/forward/side/extra/task
                                      // 空节点 trigger-buttons 表示只留拖尾、不触发爆裂

    hide-in-fullscreen false          // 全屏窗口上隐藏（上游默认不隐藏）
    show-on-desktop true              // 空工作区（桌面）上显示
    touch-mode false                  // 触摸屏模式

    disk {                            // 中心圆盘
        radius 64.8
        lifetime 200
    }
    rings {                           // 溶解环
        count 2
        lifetime 600
        radius-min 68.93
        radius-max 80.41
        spin 11.17
        spin-direction -1             // 1 或 -1，0 冻结
        inner-ratio 0.9401275         // 越接近 1 越细
        hdr-intensity 5.992157        // >1 会过曝成 bloom
    }
    shards {                          // 点击飞溅的三角碎片（拖尾碎片由 trail 控制）
        count 4
        lifetime-min 600
        lifetime-max 700
        radius 49.88
        speed-min 49.88
        speed-max 66.5
        size-min 16.63
        size-max 33.25
        hdr-intensity 5.992157
    }
    trail {                           // 光带拖尾
        width 1.0
        opacity 1.0                   // 0 关闭拖尾但保留点击爆裂
        length 1.0
    }
}
```

四个子块 `disk` / `rings` / `shards` / `trail` 各最多出现一次，只写需要改的字段即可。`glow` 段已由整帧 Bloom 取代，仍接受旧配置但不再单独生效。
把某个 `count` 或 `opacity` 设为 `0` 可以单独去掉那个元素。

#### 开关快捷键

动作名 `toggle-cursor-effect`，例如：

```kdl
Mod+M hotkey-overlay-title="开关光标特效" repeat=false { toggle-cursor-effect; }
```

### 2. shorin-niri 网格预览的开关快捷键

配置项：

```kdl
Mod repeat=false { toggle-grid-overview; }
```

`default-mod-action` 这个设置项已移除。

## 构建与使用

本仓库自带 flake：

```bash
nix develop . --command cargo build --bin niri
# 或直接
nix build .#niri
```

在 flake 里作为输入使用：

```kdl
inputs.niri-mioha.url = "github:yigexuanmu/niri-mioha";
```

## 许可证

本仓库主体（niri 派生部分）按 **GPL-3.0-or-later** 发布，完整文本见根目录
[`LICENSE`](LICENSE) 与 [`LICENSES/GPL-3.0-or-later.txt`](LICENSES/GPL-3.0-or-later.txt)。

`src/cursor_effect/` 的光标特效模块移植/衍生自
[CialloKing/ba-click-fx-desktop](https://github.com/CialloKing/ba-click-fx-desktop)
（**GPL-2.0-only**），包括 `ba/` 的模拟、曲线、条带与 LZ4/纹理代码，
`ba_data/` 的四张纹理压缩数据，以及着色器中的对应材质/Bloom 部分。

**重要限制**：GPL-2.0-only 与 GPL-3.0 不兼容。当前仓库作为整体
（GPL-3.0 主体 + GPL-2.0-only 模块）的分发需要先取得上游双许可、拆分独立
进程或重写相关层；在完成前，请不要把合并后的二进制或源码包视为已获得分发
授权。第三方归属、涉及文件和完整说明见
[`THIRD-PARTY-NOTICES.txt`](THIRD-PARTY-NOTICES.txt)。

## 追踪上游

两个远程已经配好，更新时：

```bash
git fetch upstream shorin
git merge upstream/main      # 官方
git merge shorin/main        # SHORiN 分支
```

本仓库同时保留了上游的完整 git 历史，`git log` 可以直接看到每次合并点。

## 上游项目

- 官方 niri：[niri-wm/niri](https://github.com/niri-wm/niri)
- SHORiN 分支：[SHORiN-KiWATA/niri](https://github.com/SHORiN-KiWATA/niri)
- 鼠标特效参考实现：[DoomVoss/BASpark](https://github.com/DoomVoss/BASpark)
- 旧分支（液态玻璃实验室版，已停止维护，代码已不在此仓库）：
  [yigexuanmu/Niri-glass](https://github.com/yigexuanmu/Niri-glass)
