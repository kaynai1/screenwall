import { invoke } from "@tauri-apps/api/core";
import { isTauri } from "./api";

/** 与服务器 `AuditRow` 一一对应（camelCase）。 */
export type AuditEntry = {
  id: number;
  sessionId: number;
  /** 主控端设备 ID。 */
  controllerDeviceId: string;
  /** 被控端设备 ID。 */
  controlledDeviceId: string;
  /** Unix 毫秒。 */
  startedAt: number;
  /** 会话尚未结束时为 null。 */
  endedAt: number | null;
  durationMs: number;
  bytesUp: number;
  bytesDown: number;
  /**
   * `relay`（账号中转）或 `direct`（局域网直连）。
   * 旧服务器不返回这个字段，按中转处理。
   */
  mode?: "relay" | "direct";
};

export type AuditPage = {
  audits: AuditEntry[];
  /** 满足过滤条件的总条数。 */
  total: number;
};

export type AuditQuery = {
  limit?: number;
  offset?: number;
  deviceId?: string | null;
};

const DAY = 24 * 60 * 60 * 1000;

/** 浏览器预览用的假数据：覆盖进行中、正常结束、长时间会话三种形态。 */
function mockPage(query: AuditQuery): AuditPage {
  const now = Date.now();
  const seed: AuditEntry[] = [
    {
      id: 12,
      sessionId: 12,
      controllerDeviceId: "studio-main",
      controlledDeviceId: "render-node",
      startedAt: now - 6 * 60 * 1000,
      endedAt: null,
      durationMs: 6 * 60 * 1000,
      bytesUp: 41 * 1024 * 1024,
      bytesDown: 3 * 1024 * 1024,
    },
    {
      id: 11,
      sessionId: 11,
      controllerDeviceId: "living-htpc",
      controlledDeviceId: "studio-main",
      startedAt: now - 3 * 60 * 60 * 1000,
      endedAt: now - 2 * 60 * 60 * 1000,
      durationMs: 62 * 60 * 1000,
      bytesUp: 8 * 1024 * 1024,
      bytesDown: 0,
    },
    {
      id: 10,
      sessionId: 10,
      controllerDeviceId: "studio-main",
      controlledDeviceId: "thinkpad",
      startedAt: now - 2 * DAY,
      endedAt: now - 2 * DAY + 24 * 60 * 1000,
      durationMs: 24 * 60 * 1000,
      bytesUp: 220 * 1024 * 1024,
      bytesDown: 16 * 1024 * 1024,
    },
  ];
  const needle = (query.deviceId ?? "").trim();
  const rows = needle
    ? seed.filter(
        (row) =>
          row.controllerDeviceId === needle || row.controlledDeviceId === needle,
      )
    : seed;
  const offset = Math.max(0, query.offset ?? 0);
  const limit = Math.min(500, Math.max(1, query.limit ?? 50));
  return {
    audits: rows.slice(offset, offset + limit),
    total: rows.length,
  };
}

/** 拉取会话审计记录（仅当前账号）。 */
export async function listAudits(query: AuditQuery = {}): Promise<AuditPage> {
  if (isTauri) {
    return invoke<AuditPage>("list_audits", {
      limit: query.limit ?? 50,
      offset: query.offset ?? 0,
      deviceId: query.deviceId ?? null,
    });
  }
  await new Promise((resolve) => setTimeout(resolve, 160));
  return mockPage(query);
}

/** 把毫秒时长写成「1 小时 06 分」「42 秒」这类短文本。 */
export function formatDuration(ms: number): string {
  if (ms <= 0) return "—";
  const totalSeconds = Math.round(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) {
    return `${hours} 小时 ${String(minutes).padStart(2, "0")} 分`;
  }
  if (minutes > 0) return `${minutes} 分 ${String(seconds).padStart(2, "0")} 秒`;
  return `${seconds} 秒`;
}

/** 审计记录的时间戳是 Unix 毫秒，展示成「今天 14:05」/「11-22 09:31」。 */
export function formatStarted(ms: number): string {
  if (!ms) return "—";
  const date = new Date(ms);
  const today = new Date();
  const sameDay =
    date.getFullYear() === today.getFullYear() &&
    date.getMonth() === today.getMonth() &&
    date.getDate() === today.getDate();
  const time = date.toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });
  if (sameDay) return `今天 ${time}`;
  const day = date.toLocaleDateString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
  });
  return `${day} ${time}`;
}
