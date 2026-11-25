# ScreenWall 界面设计系统

本文件是 `ui/src` 的唯一视觉依据。所有页面、组件、状态都必须从这里取值，不再新增颜色、圆角、阴影或字号。

产品是**工作台工具**，不是展示页。用户打开它是要看屏幕、连设备、传文件。界面应当安静、准确、可预期，把注意力留给远处的屏幕内容。

- 模式：Operate（任务优先）。熟悉感优先于个性，精确优先于表达。
- 视觉语言对齐 **Apple / iOS**（参考 [lenso-ui](https://github.com/LioRael/lenso-ui) 的设计契约）：语义色分层、分组列表、大圆角、克制的阴影。
- 默认外观是**浅色**（纯白卡片 + `#f2f2f7` 分组背景）；深色在设置里手动切换，不跟随系统。
- 一个字体家族，固定字号阶梯，不做流体字号。
- 单一强调色（iOS `systemBlue`），只用于交互与状态。
- 玻璃材质只用于「浮在内容之上」的控件：顶栏、控制页底部 dock、选择工具条、toast。
- 每个可交互元素必须具备 default / hover / focus-visible / active / disabled / loading / error 中的适用状态；按下时统一 `scale(0.97)` 收缩反馈。

---

## 1. 颜色

视觉语言对齐 **Apple 语义色**（取值见 `packages/tokens` 的 `apple-colors.ts`，
与 [lenso-ui](https://github.com/LioRael/lenso-ui) 同源）。两套主题共用一份 token 名，
浅色在 `:root`、深色在 `:root[data-theme="dark"]`；切换由设置 → 外观 → 主题完成，
偏好存 `localStorage`（`screenwall.theme`），首屏在 `main.tsx` 同步应用。

**规则：样式规则里不出现字面色值，只引用 token。** 唯一的例外是视频舞台
（`--stage-bg`），两种主题下都必须近黑，否则画面边缘会透出背景色。

### 中性层

| Token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--bg` | `#f2f2f7` | `#000000` | 内容区底色（iOS 分组背景） |
| `--bg-soft` | `#ffffff` | `#1c1c1e` | 输入框、次级面板底 |
| `--card` | `#ffffff` | `#1c1c1e` | 面板、卡片、侧栏、弹层 |
| `--hover` | `rgba(120,120,128,0.08)` | `rgba(120,120,128,0.16)` | 悬停层 |
| `--active` | `rgba(120,120,128,0.16)` | `rgba(120,120,128,0.24)` | 按下、选中 |
| `--strong` | `rgba(120,120,128,0.24)` | `rgba(120,120,128,0.32)` | 强选中 |
| `--line` | `rgba(60,60,67,0.12)` | `rgba(84,84,88,0.6)` | 常规分隔线 |
| `--line-soft` | `rgba(60,60,67,0.07)` | `rgba(84,84,88,0.36)` | 次级分隔线 |
| `--line-strong` | `rgba(60,60,67,0.18)` | `rgba(84,84,88,0.8)` | 悬停边框、弹层边框 |
| `--line-hover` | `rgba(60,60,67,0.26)` | `rgba(120,120,128,0.9)` | 边框的悬停态 |

控件填充（iOS 的 `fill` 层级，半透明灰叠在任意底色上都成立）：

| Token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--fill-1` | `rgba(120,120,128,0.12)` | `rgba(120,120,128,0.24)` | 次要按钮、胶囊、图标按钮底 |
| `--fill-2` | `rgba(120,120,128,0.08)` | `rgba(120,120,128,0.16)` | 输入框、代码片段、次级卡片 |
| `--fill-3` | `rgba(120,120,128,0.2)` | `rgba(120,120,128,0.36)` | 填充底上的悬停/按下 |
| `--chrome-bg` | `rgba(255,255,255,0.72)` | `rgba(28,28,30,0.72)` | 顶栏、dock、toast 的半透明材质 |
| `--chrome-border` | `rgba(60,60,67,0.12)` | `rgba(84,84,88,0.6)` | 材质底边 |

### 文字

| Token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--text` | `#000000` | `#ffffff` | 标题、正文主色 |
| `--text-2` | `rgba(60,60,67,0.6)` | `rgba(235,235,245,0.6)` | 次级正文、按钮文字 |
| `--muted` | `rgba(60,60,67,0.6)` | `rgba(235,235,245,0.6)` | 说明、占位、时间戳 |
| `--faint` | `rgba(60,60,67,0.3)` | `rgba(235,235,245,0.3)` | 辅助信息、禁用态 |

`--text-2` / `--muted` 是 Apple 的 `label-secondary`，在 `#ffffff` 上等效约
`#3c3c43`，对比度 12.6:1；`--faint` 等效约 `#b3b3b8`，只用于非关键信息。

### 强调与语义

| Token | 浅色 | 深色 | 用途 |
|---|---|---|---|
| `--accent` | `#007aff` | `#0a84ff` | 主操作、焦点环、当前选中（iOS systemBlue） |
| `--accent-hover` | `#0071e3` | `#409cff` | 主操作悬停 |
| `--accent-soft` | `rgba(0,122,255,0.12)` | `rgba(10,132,255,0.18)` | 强调底色、选中胶囊 |
| `--accent-soft-strong` | `rgba(0,122,255,0.18)` | `rgba(10,132,255,0.26)` | 强调底色（悬停） |
| `--accent-line` | `rgba(0,122,255,0.3)` | `rgba(10,132,255,0.42)` | 强调边框 |
| `--on-accent` | `#ffffff` | `#ffffff` | 强调底上的文字/图标 |
| `--green` / `--green-soft` / `--green-line` | `#34c759` / `rgba(52,199,89,0.12)` / `rgba(52,199,89,0.32)` | `#30d158` / `rgba(48,209,88,0.16)` / `rgba(48,209,88,0.36)` | 在线、成功、开关开启 |
| `--red` / `--red-soft` / `--red-soft-strong` / `--red-line` | `#ff3b30` / `rgba(255,59,48,0.1)` / `rgba(255,59,48,0.16)` / `rgba(255,59,48,0.3)` | `#ff453a` / `rgba(255,69,58,0.14)` / `rgba(255,69,58,0.22)` / `rgba(255,69,58,0.36)` | 错误、危险操作 |
| `--amber` / `--amber-soft` / `--amber-line` | `#ff9500` / `rgba(255,149,0,0.12)` / `rgba(255,149,0,0.32)` | `#ff9f0a` / `rgba(255,159,10,0.16)` / `rgba(255,159,10,0.36)` | 连接中、需要注意 |
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
| `--r-sm` | 8px | 小徽标、图标按钮、分段控件项 |
| `--r` | 10px | 按钮、输入、导航项 |
| `--r-lg` | 14px | 次级卡片、分组内元素 |
| `--r-xl` | 18px | 大面板 |
| `--r-card` | 16px | 分组卡片、表格容器、屏幕墙设备卡 |
| `--r-pill` | 999px | 胶囊：chip、状态徽标、开关、toast |

- 胶囊用于 chip、状态徽标、开关轨道、toast，不用于主按钮。
- 不使用单侧竖条边框、不使用一侧内阴影。

---

## 4. 阴影与层级

- 卡片靠**白底 + 大圆角 + 极轻阴影**浮在分组背景上，不用描边。
  `--panel-shadow: 0 1px 3px rgba(0,0,0,.06)`。
- 浮层（弹层、菜单、toast）用 `--shadow-pop`，大而柔。
- 玻璃材质（`--chrome-bg` + `backdrop-filter: blur(20px) saturate(180%)`）只用于
  浮在内容之上的控件：顶栏、控制页底部 dock、选择工具条、toast。
  普通内容面不用玻璃。
- 禁止零偏移彩色光晕。
- 模态遮罩：`--overlay` + `blur(6px)`。

---

## 5. 动效

- 时长 `200ms`，缓动 `cubic-bezier(.32,.72,0,1)`（iOS 的进出曲线）；按下反馈 120ms。
- **按下反馈**：可点元素 `:active` 统一 `transform: scale(.97)`，松手回弹。
- 弹层入场：`scale(.96) → 1` + 淡入，260ms；遮罩淡入 180ms。
- 分段控件：选中项是白色滑块 + 细阴影，切换时颜色与阴影过渡，不做位移动画。
- 只动 `background-color`、`border-color`、`color`、`opacity`、`transform`。
- 页面切换不做入场编排；不做依次淡入、不做滚动触发动画。
- 必须有 `prefers-reduced-motion: reduce` 与 `data-motion="off"` 兜底：
  关掉缩放与位移，但保留颜色/透明度变化，功能不受影响。

---

## 6. 图标

- 只用 `lucide-react`。
- 尺寸：导航 16、按钮 14–16、面板标题 16、空状态 20。
- `strokeWidth={1.75}`，与文字同色或 `--text-3`。
- 禁止 emoji、禁止 Unicode 字符充当图标。

---

## 7. 组件规格

### 按钮 `.btn`

高 34px，内边距 `0 14px`，字号 15/500，圆角 10，图标 14。

- `.btn-primary`：`--accent` 底 + 白字；hover `--accent-hover`。
- `.btn-ghost`：`--fill-1` 填充底 + `--text` 字，无边框；hover `--fill-3`。
- `.btn-soft`：`--accent-soft` 底 + `--accent` 字。
- `.btn-danger`：`--red-soft` 底 + `--red` 字。
- `.btn-sm` 28px；`.btn-lg` 44px。
- 按下 `scale(.97)`；不在按钮上做渐变、发光。

### 输入 `.field-control`

高 38px，`--fill-2` 填充底、无边框、圆角 10，字号 15。
焦点：边框 `--accent` + `0 0 0 4px var(--focus-ring)`，底色转 `--card`。

### 面板 `.panel`

白底、无边框、圆角 16、内边距 18、`--panel-shadow`。面板不嵌套面板。

### 徽标 `.badge`

胶囊（`--r-pill`），字号 13，内边距 `2px 8px`，无边框，语义色浅底。
在线用 `--green`，警告用 `--amber`，错误用 `--red`，中性用 `--fill-1` + `--text-2`。

### 列表行

- 行高 ≥ 52px，分隔线用 `::before` 伪元素从左侧 16px 处开始（iOS inset 分隔线）。
- 可点行按下整行变 `--active`；右侧用 `ChevronRight` 表示可进入。

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
