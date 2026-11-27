/**
 * 来自 QingIcon（https://qingicon.com，line 风格，24×24、2px 描边）的少量图标。
 *
 * 只把链路相关的几个挑出来做成内联组件：这套图的线宽与圆角跟项目里
 * Lucide 的 1.75px 线风格接近，放在一起不突兀；直接内联也省掉一个新依赖。
 * 图标路径取自 `assets.qingicon.com/icons/line/<分类>.json`。
 */
type IconProps = {
  /** 与 Lucide 一致，允许数字或带单位的字符串。 */
  size?: number | string;
  strokeWidth?: number | string;
  className?: string;
};

function Glyph({
  size = 16,
  strokeWidth = 1.75,
  className,
  d,
}: IconProps & { d: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      width={size}
      height={size}
      fill="none"
      className={className}
      aria-hidden="true"
    >
      <path
        d={d}
        stroke="currentColor"
        strokeWidth={strokeWidth}
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** 局域网直连：路由器。 */
export function RouterIcon(props: IconProps) {
  return (
    <Glyph
      {...props}
      d="M7 3.5v9m5-9v9m5-9v9m-5.75 4h.01m-4.01 0h.01M3 12.5h18v8H3zm8.5 4a.25.25 0 1 1-.5 0 .25.25 0 0 1 .5 0m-4 0a.25.25 0 1 1-.5 0 .25.25 0 0 1 .5 0"
    />
  );
}

/** 服务器中转：云 + 服务器。 */
export function RelayIcon(props: IconProps) {
  return (
    <Glyph
      {...props}
      d="M18 9.352V9.25a6 6 0 1 0-12 .102m12-.002a5.002 5.002 0 0 1 .952 9.505M6 9.35a5.002 5.002 0 0 0-.952 9.505M9 16.5h6m-6-4h6v8H9z"
    />
  );
}

/** P2P 直连：节点之间点对点相连。 */
export function P2pIcon(props: IconProps) {
  return (
    <Glyph
      {...props}
      d="m6.219 16.105 1.562-5.21m2.487-.627 3.464 3.464m2.486-.627 1.563-5.21M8 18.5a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0m13-13a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0m-10 3a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0m7 7a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0"
    />
  );
}

/** 连接码会话：两个圆点用一条线连起来。 */
export function LinkCodeIcon(props: IconProps) {
  return (
    <Glyph
      {...props}
      d="M9.121 14.879 14.88 9.12M20 7a3 3 0 1 1-6 0 3 3 0 0 1 6 0M10 17a3 3 0 1 1-6 0 3 3 0 0 1 6 0"
    />
  );
}

/** 建议与 BUG：小虫子。 */
export function BugIcon(props: IconProps) {
  return (
    <Glyph
      {...props}
      d="M19 14h3m-3 0V9H5v5m14 0a7 7 0 0 1-7 7m5.86-3.183 2.07.591L21 20M6.138 17.818l-2.07.591L3 20m16-9.25L22 9M5 10.75 2 9m0 5h3m0 0a7 7 0 0 0 7 7m0 0v-6m5-6V8A5 5 0 0 0 7 8v1z"
    />
  );
}
