// 活跃远程会话登记：控制页和文件页共用同一条会话。
//
// 控制页卸载时不再立刻断开，而是把会话交给这里；只有离开这两个页面
// 或用户主动重连时才真正调用 stopControl，避免「看文件就得重新授权」。

import { stopControl } from "./api";

type Active = { deviceId: string; token: number };

let active: Active | null = null;

/**
 * 直连设备的合成 ID：后端用 `direct:<地址>:<端口>`，演示模式用 `direct-...`。
 * 这类 ID 不在账号设备列表里，控制页要靠 `getControlInfo` 复用会话。
 */
export function isDirectDeviceId(deviceId: string): boolean {
  return deviceId.startsWith("direct:") || deviceId.startsWith("direct-");
}

export function rememberSession(deviceId: string, token: number) {
  active = { deviceId, token };
}

export function forgetSession(token: number) {
  if (active?.token === token) active = null;
}

export function activeSessionDevice(): string | null {
  return active?.deviceId ?? null;
}

/** 离开控制/文件页且不再需要画面时调用；幂等。 */
export async function stopActiveSession() {
  const current = active;
  active = null;
  if (!current) return;
  await stopControl(current.token).catch(() => {});
}
