# ScreenWall 界面设计系统

本文件是 `ui/src` 的唯一视觉依据。所有页面、组件、状态都必须从这里取值，不再新增颜色、圆角、阴影或字号。

产品是**工作台工具**，不是展示页。用户打开它是要看屏幕、连设备、传文件。界面应当安静、准确、可预期，把注意力留给远处的屏幕内容。

- 模式：Operate（任务优先）。熟悉感优先于个性，精确优先于表达。
- 一个字体家族，固定字号阶梯，不做流体字号。
- 单一强调色，只用于交互与状态；主操作按钮用高对比中性色。
- 无玻璃、无光晕、无扫描线、无渐变文字、无装饰性动效。
- 每个可交互元素必须具备 default / hover / focus-visible / active / disabled / loading / error 中的适用状态。

---

## 1. 颜色

视觉语言与 **KayPanel**（`web/assets/style.css`）对齐：同一组中性层、语义色与圆角，
同一套字体栈。两套主题共用一份 token 名，浅色在 `:root`、深色在
`:root[data-theme="dark"]`；切换由设置 → 连接 → 外观 → 主题完成，
偏好存 `localStorage`（`screenwall.theme`），首屏在 `main.tsx` 同步应用。

**规则：样式规则里不出现字面色值，只引用 token。** 唯一的例外是视频舞台
（`--stage-bg`），两种主题下都必须近黑，否则画面边缘会透出背景色。

### 中性层

| Token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--bg` | `#f4f5f7` | `#0a0f17` | 内容区底色 |
| `--bg-soft` | `#f8f9fb` | `#111827` | 输入框、次级面板底 |
| `--card` | `#ffffff` | `#121a26` | 面板、卡片、侧栏、弹层 |
| `--hover` | `#f5f6f8` | `#182233` | 悬停层 |
| `--active` | `#eceef1` | `#1d293a` | 按下、选中 |
| `--strong` | `#e3e6eb` | `#243247` | 强选中、分段控件活动项 |
| `--line` | `#e7e9ee` | `#202b3a` | 常规分隔线、边框 |
| `--line-soft` | `#eff1f4` | `#1a2433` | 次级分隔线 |
| `--line-strong` | `#d9dde4` | `#2d3a4d` | 悬停边框、弹层边框 |
| `--line-hover` | `#cfd4dc` | `#334155` | 边框的悬停态 |

### 文字

| Token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--text` | `#0f172a` | `#e8eef7` | 标题、正文主色 |
| `--text-2` | `#334155` | `#c2cddb` | 次级正文、按钮文字 |
| `--muted` | `#52606f` | `#93a1b5` | 说明、占位、时间戳 |
| `--faint` | `#667085` | `#7c8ba1` | 辅助信息、禁用态 |

实测对比度（卡片底 vs 文字，2026-11-24）：浅色 muted 6.44:1 / faint 4.97:1，
深色 muted 6.66:1 / faint 5.05:1；正文两套都在 14.9:1 以上，均过 WCAG AA。

### 强调与语义

| Token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--accent` | `#2563eb` | `#60a5fa` | 主操作、焦点环、当前选中 |
| `--accent-hover` | `#1d4ed8` | `#3b82f6` | 主操作悬停 |
| `--accent-soft` | `#eef4ff` | `#16233a` | 强调底色 |
| `--accent-soft-strong` | `#dee9ff` | `#1b2f4f` | 强调底色（悬停） |
| `--accent-line` | `#c7dbff` | `#2a4468` | 强调边框 |
| `--on-accent` | `#ffffff` | `#0a0f17` | 强调底上的文字/图标 |
| `--green` / `--green-soft` / `--green-line` | `#16a34a` / `#e9f7ef` / `#cdebd8` | `#34d399` / `#0f2a1e` / `#1c4436` | 在线、成功 |
| `--red` / `--red-soft` / `--red-soft-strong` / `--red-line` | `#dc2626` / `#fef2f2` / `#fee2e2` / `#f3cdcd` | `#f87171` / `#2a1518` / `#3a1a1f` / `#4a2328` | 错误、危险操作 |
| `--amber` / `--amber-soft` / `--amber-line` | `#b45309` / `#fff7e8` / `#f2ddb5` | `#fbbf24` / `#2a2312` / `#4a3a18` | 连接中、需要注意 |
| `--stage-bg` | `#050607` | `#050607` | 视频舞台（两套主题相同） |

### 兼容别名

旧规则里用的名字保留为别名，指向新 token，避免一次性改所有页面：

`--bg-sidebar` → `--card`、`--surface` → `--card`、`--surface-2` → `--bg-soft`、
`--surface-3` → `--active`、`--surface-4` → `--strong`、`--text-3` → `--muted`、
`--text-mute` → `--faint`、`--accent-strong` → `--accent`、
`--accent-dim` → `--accent-soft`、`--ok` → `--green`、`--warn` → `--amber`、
`--danger` → `--red`、`--info` → `--accent`。

新代码请直接用新名字。

规则：

- 强调色只用于主操作、当前项、焦点、状态点，不做大面积装饰底色。
- 在线状态用 `--green` 圆点 + 文字，不用发光。
- 正文与占位文字对比度 ≥ 4.5:1；大号文字 ≥ 3:1。
- 不在彩色底上使用灰色文字；需要次级文字时用同色系降低明度。

---

## 2. 字体

```css
:root {
  --font: "Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei", "PingFang SC",
    "Hiragino Sans GB", "Noto Sans CJK SC", system-ui, sans-serif;
  --mono: "Cascadia Mono", Consolas, "SFMono-Regular", "Noto Sans Mono CJK SC",
    monospace;
}

body {
  font-family: var(--font);
  font-size: 15px;
  line-height: 1.6;
  font-variant-numeric: tabular-nums lining-nums;
}
```

- 正文字号取整数 15px（不要 14.5px）：半像素字号在 100% 缩放下会重新引入糊边。
  KayPanel 用的是 14.5 / 13.5 / 12.5 / 11.5 的半像素阶梯，这里整体上取整到
  15 / 14 / 13 / 12，比例关系保持不变；这是两套界面唯一有意保留的差异。
- 只有路径、端口、IP、设备 ID、日志等技术文本用 `--mono`（`.mono` 类），正文与界面文案不混用等宽。
- 数字统一 `tabular-nums lining-nums`，刷新时列宽不跳动。

固定阶梯（px，不用 clamp）：

| 级别 | 字号/行高 | 字重 | 用途 |
|---|---|---|---|
| Display | 26 / 1.25 | 600 | 仅登录页主张 |
| Title | 20 / 1.35 | 600 | 页面内主分区标题 |
| Section | 17 / 1.4 | 600 | 面板标题、状态强调 |
| Title-bar | 16 / 1.4 | 600 | 顶栏页面标题 |
| Body | 15 / 1.55 | 400 | 正文 |
| Control | 15 / 1.4 | 500 | 按钮、输入、导航 |
| Meta | 14 / 1.5 | 400 | 说明、标签、辅助信息 |
| Micro | 13 / 1.4 | 500 | 徽标、计数，仅此用途 |

规则：

- 字间距 `0`；只有用户名/编号等短标识可用 `0.02em`。
- 数据数字使用 `font-variant-numeric: tabular-nums`。
- 不使用大写英文 eyebrow、kicker、章节编号。
- 中文正文行高不低于 1.55；说明文字宽度不超过 68 个汉字。
- 界面最小字号 13px，正文和数据不低于 14px；不使用半像素字号。
- 字体栈以系统 `Segoe UI` / `Microsoft YaHei UI` 为准，不引入网络字体；不使用 `-webkit-font-smoothing`。
- 禁止渐变文字、禁止等宽字体作为「技术感」装饰。

### 文本渲染（Windows / WebView2）

- 客户端在 `crates/client/tauri.conf.json` 的 `additionalBrowserArgs` 里固定了
  `--disable-lcd-text --disable-font-subpixel-positioning`。
- 实测（100% DPI，4 倍放大截图）：开启 LCD 子像素渲染时，深色底上的字形边缘出现
  明显的橙/蓝彩色条纹，1 倍观看时整行文字发虚、发紫；改为灰度抗锯齿并把字形位置
  对齐到整像素后，边缘变为中性灰，笔画边界清楚，观感才「实」。
- 该参数是**整个附加参数串**，修改时必须保留
  `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection`。
- 根节点 `html / body / #root` 保持不透明背景，避免合成层与底色叠加带来的边缘杂色。
- 不要使用 `-webkit-font-smoothing`（Windows 上无效，只会误导）。
- 深色底上的细字容易显虚：正文不低于 15px、辅助信息不低于 14px；正文字重保持
  400，不靠加粗补救。

---

## 3. 间距与圆角

间距阶梯：`4 / 8 / 12 / 16 / 20 / 24 / 32 / 40`。

- 组内元素：8–12px
- 区块之间：24px
- 页面上边距：24px，左右 28px，底部 32px
- 标题与其下内容之间 12px；标题上方留白大于下方

圆角：

| Token | 值 | 用途 |
|---|---|---|
| `--r-sm` | 7px | 小徽标、图标按钮、分段控件项 |
| `--r` | 8px | 按钮、输入、导航项 |
| `--r-lg` | 12px | 面板、卡片、弹层内容块 |
| `--r-xl` | 14px | 仅模态框 |

- 胶囊形只用于状态徽标，不用于按钮。
- 不使用单侧竖条边框、不使用一侧内阴影。

---

## 4. 阴影与层级

- 平面内容靠**边框 + 中性层**区分层级，不靠阴影。
- 只有浮层使用阴影：`--shadow-pop: 0 18px 44px rgba(0,0,0,.55)`。
- 禁止零偏移彩色光晕、禁止 `backdrop-filter` 装饰性模糊。
- 模态遮罩：`rgba(0,0,0,.6)`，不加模糊。

---

## 5. 动效

- 时长 `160ms`，缓动 `cubic-bezier(.2,.7,.3,1)`；弹层 180ms。
- 只动 `background-color`、`border-color`、`color`、`opacity`、`transform`（≤4px 位移或缩放 ≥.98）。
- 页面切换不做入场编排；不做依次淡入、不做滚动触发动画。
- 必须有 `prefers-reduced-motion: reduce` 兜底。

---

## 6. 图标

- 只用 `lucide-react`。
- 尺寸：导航 16、按钮 14–16、面板标题 16、空状态 20。
- `strokeWidth={1.75}`，与文字同色或 `--text-3`。
- 禁止 emoji、禁止 Unicode 字符充当图标。

---

## 7. 组件规格

### 按钮 `.btn`

高 34px，内边距 `0 12px`，字号 13/500，圆角 8，图标 14。

- `.btn-primary`：底 `#f1f2f4`，字 `#0b0c0e`；hover 纯白；disabled 透明度 .45。
- `.btn-ghost`：透明底 + `--line` 边框 + `--text-2` 字；hover 底 `--surface-2`、边框 `--line-strong`、字 `--text`。
- `.btn-soft`：`--accent-dim` 底 + `--accent-strong` 字。
- `.btn-danger`：`rgba(229,103,103,.10)` 底 + `#f08c8c` 字 + 同色 22% 边框。
- `.btn-sm` 28px；`.btn-lg` 42px。
- 不在按钮上做渐变、发光、位移超过 1px。

### 输入 `.field-control`

高 36px，底 `--surface-2`，边框 `--line`，圆角 8，字号 13。焦点：边框 `rgba(142,162,255,.6)` + `0 0 0 3px rgba(142,162,255,.14)`。

### 面板 `.panel`

底 `--surface`，边框 `--line`，圆角 12，内边距 16。面板不嵌套面板。

### 徽标 `.badge`

圆角 4，字号 11，内边距 `1px 6px`。在线用 `--ok`，警告用 `--warn`，错误用 `--danger`，中性用 `--text-3`。不用胶囊。

### 表格 / 列表

- 行高 ≥ 44px，行间用 1px `--line` 分隔，不用卡片包每行。
- 表头 12px `--text-3`，正文 13px。
- 悬停整行底色 `--surface-2`。
- 空数据：说明 + 一个明确操作，不用插画。

### 弹层

- 只有需要打断或保护焦点时才用。优先内联展开、侧栏、抽屉。
- `Modal` 组件保持 API 不变。

---

## 8. 页面结构

- 顶栏是唯一的页面标题来源（约 15px）。页面主体**不再重复大标题**。
- 页面顶部是一条工具栏：左侧筛选/视图切换，右侧主操作。
- 一屏内最多一层卡片；优先用分区标题 + 行列表 + 表格。
- 同尺寸「图标 + 标题 + 说明」卡片网格不作为页面结构。
- 设备/文件等实体用列表行，不用大卡片堆叠。

### 左侧导航

236px，`--bg-sidebar` 底，右侧 1px 分隔线。品牌区 56px 与顶栏对齐。导航项高 34px，选中态为 `--surface-3` 底 + 白字，不加竖条、不加光晕。分组标题 11px `--text-mute`，不用大写。

### 顶栏

56px，`--bg` 底，下边 1px 分隔线。左侧页面标题，右侧连接状态、帮助、通知、头像。状态胶囊：12px 文字，圆点表示状态，不用彩色发光。

---

## 9. 空、错、载入状态

- 载入：骨架块（`--surface-2`，1.2s 轻微明暗），不用居中转圈。
- 空：一句说明 + 主操作。
- 错：说明问题 + 恢复动作（重试 / 重新登录 / 检查服务器）。
- 断流、离线、连接中必须同时有文字和颜色，不能只靠颜色。

---

## 10. 禁止清单

- 玻璃拟态、装饰性 `backdrop-filter`、彩色光晕、扫描线、噪点、网格背景。
- 渐变文字、渐变按钮、渐变进度条。
- 页面大标题上方的 eyebrow / kicker、01/02/03 编号。
- 同尺寸卡片网格、卡片套卡片、装饰性假 UI 预览。
- emoji 图标、Unicode 符号当图标。
- 平台默认焦点环不主题化、默认滚动条不主题化。
- 只做 default 态的交互控件。
