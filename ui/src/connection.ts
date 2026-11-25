import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { isTauri } from "./api";

/** 看门狗重启会话任务时发的事件（`tray::watchdog_loop`）。 */
export type WatchdogEvent = {
  action: string;
  reason: string;
};

/**
 * 订阅连接看门狗事件。看门狗在主控/被控连续约 2 分钟未连上时
 * 重启会话任务，界面需要提示「正在重连」，而不是静默卡在「连接中」。
 */
export function subscribeWatchdog(
  handler: (event: WatchdogEvent) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<WatchdogEvent>("watchdog", (event) => handler(event.payload))
    .then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    })
    .catch(() => {});
  return () => {
    disposed = true;
    unlisten?.();
  };
}
