/**
 * 界面外观：主题与动效。
 *
 * 两者都只影响本机界面，存在 `localStorage` 里，不写进 `client.json`：
 * 换机器不该带着别人的显示偏好。首屏在 `main.tsx` 里同步应用一次，
 * 避免启动后再闪一下（主题闪白在深色屏上尤其明显）。
 *
 * 主题用 `data-theme` 挂在 `<html>` 上，颜色全部由 `styles.css` 的
 * 两套 token 提供；不在这里写任何色值。
 */

export type Theme = "light" | "dark";

const THEME_KEY = "screenwall.theme";
const MOTION_KEY = "screenwall.motion";

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function write(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // 存储不可用（隐私模式、配额满）时只影响本次会话，忽略。
  }
}

/**
 * 没有存过偏好时一律用浅色。
 *
 * 之前跟随系统 `prefers-color-scheme`，系统是深色时软件一打开就是全黑，
 * 与「打开即纯白」的预期不符；深色仍可在设置 → 外观里手动选。
 */
export function storedTheme(): Theme {
  const saved = read(THEME_KEY);
  if (saved === "light" || saved === "dark") return saved;
  return "light";
}

export function applyTheme(theme: Theme, persist = true) {
  document.documentElement.dataset.theme = theme;
  if (persist) write(THEME_KEY, theme);
}

export function storedMotion(): boolean {
  return read(MOTION_KEY) !== "off";
}

export function applyMotion(enabled: boolean, persist = true) {
  document.documentElement.dataset.motion = enabled ? "on" : "off";
  if (persist) write(MOTION_KEY, enabled ? "on" : "off");
}

/** 首屏调用：应用两项偏好，不写回存储（用户没改就不该落盘）。 */
export function bootstrapAppearance() {
  applyTheme(storedTheme(), false);
  applyMotion(storedMotion(), false);
}
