# niri-mioha

个人自用的 niri 分支：**SHORiN niri + 官方 niri 最新代码**，外加一套自研的**光标粒子特效**。

> ⚠️ 个人向分支，不是发行版。功能以我自己的使用习惯为准，随时可能改动。
> 想要稳定干净的 niri，请用 [niri-wm/niri](https://github.com/niri-wm/niri)。

## 这个仓库是什么

本仓库是**从 SHORiN niri 的最新 main 重新起底**建立的，不是旧 `Niri-glass` 分支的延续：

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
- **液态玻璃（liquid glass）已彻底移除**：旧 `Niri-glass` 的玻璃补丁集
  （`liquid_glass.rs`、`lg_*` uniform、`clipped_surface.frag` 里的折射/泛光代码、
  `appearance` 里的 `LiquidGlass` 配置）全部不存在，仓库里 grep 不到任何 `glass`。
- **保留本分支独有功能**：光标粒子特效，实现参考 [BASpark](https://github.com/DoomVoss/BASpark)，
  全部用 GLES 在合成器内绘制，不依赖外部程序。

## 相对上游的两处本地改动

### 1. 光标粒子特效（本分支主要亮点）

参考 BASpark 实现的矢量粒子特效，渲染成**代码字符**（系统字体栅格化到 atlas，
配合稳定的伪随机种子，每次点击的字符、速度、半径、淡出时间都不同）。

<details><summary>[展开/收起] 效果预览</summary>
<br>

- 左键

<img width="960" alt="左键爆裂" src="https://raw.githubusercontent.com/yigexuanmu/images/main/Niri-beta/%E5%B7%A6%E9%94%AE.gif" />

- 右键

<img width="960" alt="右键爆裂" src="https://raw.githubusercontent.com/yigexuanmu/images/main/Niri-beta/%E5%8F%B3%E9%94%AE.gif" />

- 中键

<img width="960" alt="中键爆裂" src="https://raw.githubusercontent.com/yigexuanmu/images/main/Niri-beta/%E4%B8%AD%E9%94%AE.gif" />

- 滚轮向下

<img width="960" alt="滚轮向下（橙色顺时针）" src="https://raw.githubusercontent.com/yigexuanmu/images/main/Niri-beta/%E6%BB%91%E8%BD%AE%E5%90%91%E4%B8%8B.gif" />

- 滚轮向上

<img width="960" alt="滚轮向上（绿色逆时针）" src="https://raw.githubusercontent.com/yigexuanmu/images/main/Niri-beta/%E6%BB%91%E8%BD%AE%E5%90%91%E4%B8%8A.gif" />

- 滚轮粒子数量与滚动速度关联

<img width="960" alt="滑轮粒子效果与速度关联" src="https://raw.githubusercontent.com/yigexuanmu/images/main/Niri-beta/%E6%BB%91%E8%BD%AE%E7%B2%92%E5%AD%90%E6%95%88%E6%9E%9C%E5%92%8C%E6%BB%91%E8%BD%AE%E9%80%9F%E5%BA%A6%E7%9A%84%E5%85%B3%E8%81%94.gif" />

- 拖尾

<img width="960" alt="拖尾" src="https://raw.githubusercontent.com/yigexuanmu/images/main/Niri-beta/%E6%BB%91%E5%8A%A8.gif" />

</details>

#### 点击爆裂

- 左键：**蓝色**（配置 `color`）
- 右键：**浅红色**
- 中键：**浅黄色**
- 每种按键的爆裂各自保存颜色快照，可同时在不同位置渲染，互不覆盖
- 圆盘 + 外圈环由代码字符组成，每字符独立的大小 / 速度 / 淡出窗口 → 逐个不规则消失
- 中键长按同样会出拖尾

#### 滚轮代码圆环

- 滚轮**向下**：淡**橙**色圆环**顺时针**旋转
- 滚轮**向上**：淡**绿**色圆环**逆时针**旋转
- 旋转速度跟随滚轮滚动的快慢（按位移量比例注入）
- 转动时从环缘沿切向**甩出火花**，力度随机散布
- 圆环在可见期间始终**跟随光标**；停止滚动后惯性旋转片刻，再逐个字符淡出消失

#### 拖尾

- 按住鼠标拖动产生密集代码字符拖尾（~2px 采样）
- 拖尾火花淡出较慢，点击火花淡出较快

#### 配置

在 niri 配置里添加 `cursor-effect { ... }` 块（全部可选，缺省取默认值）：

```kdl
cursor-effect {
    enabled true                     // 总开关（默认 true）
    scale 1.5                        // 特效缩放（默认 1.5）
    opacity 1.0                      // 整体不透明度（默认 1.0）
    color "45,175,255"               // 左键爆裂颜色，R,G,B 或 #rrggbb（默认 "45,175,255"）
    effect-speed 1.0                 // 动画速度基准（默认 1.0）
    use-linked-animation-speed true  // 拖尾/点击共用 effect-speed（默认 true）
    trail-speed 1.0                  // 拖尾速度（use-linked 时被忽略）
    click-speed 1.0                  // 点击速度（use-linked 时被忽略）
    trail-refresh-rate 40            // 拖尾采样频率 Hz（默认 40）
    enable-always-trail false        // 不按键也常显拖尾（默认 false）
    apply-curve-draw false           // 拖尾曲线平滑（默认 false）
    click-trigger "both"             // 哪些按键触发爆裂：left / right / both（默认 left）
    enable-middle-click-trigger true // 中键是否触发（默认 true）
    hide-in-fullscreen true          // 全屏时隐藏特效（默认 true）
    show-on-desktop true             // 桌面上显示特效（默认 true）
    touch-mode false                 // 触摸屏模式（默认 false）
}
```

#### 开关快捷键

动作名 `toggle-cursor-effect`，例如：

```kdl
Mod+M hotkey-overlay-title="开关光标特效" repeat=false { toggle-cursor-effect; }
```

### 2. 网格预览的开关改成普通按键绑定

SHORiN niri 原本把「点击 Mod 键切换网格预览」硬编码在 `input/mod.rs` 里
（`pending_default_mod_tap`），只受 `grid-overview { default-mod-action }` 控制，
既不能改键也不能删掉。

本分支把它改成**普通 binds 条目**，默认配置随附：

```kdl
Mod repeat=false { toggle-grid-overview; }
```

也就是默认行为不变，但可以像其它按键一样随意改键，或者删掉这行彻底关掉这个入口。
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
- 旧分支（液态玻璃版，已停止维护，代码已不在此仓库）：
  [yigexuanmu/Niri-glass](https://github.com/yigexuanmu/Niri-glass)
