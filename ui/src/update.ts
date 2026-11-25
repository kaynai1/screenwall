// 自托管自动更新：包装 Tauri 命令与进度事件。
//
// 浏览器预览（非 Tauri）下没有更新能力，检查更新固定返回「已是最新」，
// 避免设置页出现无法工作的按钮。

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { isTauri } from "./api";

/** 与 Rust 端 `update::UpdateInfo` 一一对应（camelCase）。 */
export type UpdateInfo = {
  /** 当前运行版本。 */
  current: string;
  /** 服务器发布的新版本。 */
  version: string;
  /** 更新说明，可能包含换行。 */
  notes: string;
  /** 安装包绝对下载地址。 */
  url: string;
  /** 安装包 SHA256（64 位小写十六进制）。 */
  sha256: string;
  /** 发布时间（Unix 毫秒）。 */
  publishedAt: number;
};

export type UpdateProgress = {
  received: number;
  total: number;
};

/** 浏览器预览用的假版本号，与 Cargo / tauri 配置一致。 */
const PREVIEW_VERSION = "0.1.0";

/** 当前运行版本；浏览器预览返回固定值。 */
export async function currentVersion(): Promise<string> {
  if (!isTauri) return PREVIEW_VERSION;
  try {
    return await getVersion();
  } catch {
    return PREVIEW_VERSION;
  }
}

/**
 * 检查更新。`null` 表示已是最新版本。
 *
 * 服务器未配置更新（404）会抛错，由调用方展示提示。
 */
export async function checkUpdate(): Promise<UpdateInfo | null> {
  if (!isTauri) return null;
  return invoke<UpdateInfo | null>("check_update");
}

/**
 * 下载并校验安装包，返回本地路径。
 *
 * 进度通过 `subscribeUpdateProgress` 推送；校验失败时后端会删除文件并抛错。
 */
export async function downloadUpdate(
  url: string,
  sha256: string,
): Promise<string> {
  if (!isTauri) throw new Error("浏览器预览模式不支持下载更新");
  return invoke<string>("download_update", { url, sha256 });
}

/** 启动安装程序（NSIS `/S`）；是否退出应用由后端命令决定。 */
export async function installUpdate(path: string): Promise<void> {
  if (!isTauri) throw new Error("浏览器预览模式不支持安装更新");
  return invoke<void>("install_update", { path });
}

/** 订阅下载进度，返回取消订阅函数。浏览器预览下为空操作。 */
export function subscribeUpdateProgress(
  handler: (progress: UpdateProgress) => void,
): () => void {
  if (!isTauri) return () => {};
  let unlisten: UnlistenFn | null = null;
  let disposed = false;
  listen<UpdateProgress>("update-progress", (event) => handler(event.payload))
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
