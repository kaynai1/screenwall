import { invoke } from "@tauri-apps/api/core";
import { isTauri } from "./api";

/** 日志占用情况，对应 Rust 端 `get_log_info`。 */
export type LogInfo = {
  dir: string;
  files: number;
  bytes: number;
};

const DEMO_INFO: LogInfo = {
  dir: "C:\\Users\\你\\AppData\\Roaming\\ScreenWall\\logs",
  files: 3,
  bytes: 512 * 1024,
};

export async function getLogInfo(): Promise<LogInfo> {
  if (isTauri) return invoke<LogInfo>("get_log_info");
  await new Promise((resolve) => setTimeout(resolve, 120));
  return DEMO_INFO;
}

/** 在资源管理器里打开日志目录，返回实际路径。 */
export async function openLogDir(): Promise<string> {
  if (isTauri) return invoke<string>("open_log_dir");
  await new Promise((resolve) => setTimeout(resolve, 80));
  return DEMO_INFO.dir;
}

/** 导出诊断包（日志 + 脱敏配置），返回目录路径。 */
export async function exportDiagnostics(): Promise<string> {
  if (isTauri) return invoke<string>("export_diagnostics");
  await new Promise((resolve) => setTimeout(resolve, 300));
  return "C:\\Users\\你\\AppData\\Roaming\\ScreenWall\\diagnostics-20261124-210000";
}

/** 关闭窗口时是否缩到托盘。 */
export async function setCloseToTray(enabled: boolean): Promise<boolean> {
  if (isTauri) return invoke<boolean>("set_close_to_tray", { enabled });
  await new Promise((resolve) => setTimeout(resolve, 80));
  return enabled;
}

/** 读本机设置里的托盘开关（与 api.ts 的 getLocalSettings 同一个命令）。 */
export async function getCloseToTray(): Promise<boolean> {
  if (!isTauri) return true;
  try {
    const settings = await invoke<{ closeToTray?: boolean }>(
      "get_local_settings",
    );
    return settings.closeToTray ?? true;
  } catch {
    return true;
  }
}
