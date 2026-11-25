import type { AccountView, DeviceView } from "./types";

const now = Date.now();

export const mockAccount: AccountView = {
  serverUrl: "http://127.0.0.1:8787",
  username: "kay",
  deviceId: "studio-main",
  loggedIn: true,
  connected: true,
  error: null,
};

export const mockDevices: DeviceView[] = [
  {
    id: "studio-main",
    name: "书房 · 主力机",
    os: "Windows 11 Pro",
    version: "24H2",
    online: true,
    lastSeen: now,
    controllable: true,
    ip: "192.168.1.20",
    resolution: "2560 × 1440",
    latency: 6,
    cpu: 18,
    mem: 42,
    gpu: "RTX 4070",
    location: "书房",
  },
  {
    id: "living-htpc",
    name: "客厅 · 影音机",
    os: "Windows 11",
    version: "23H2",
    online: true,
    lastSeen: now - 40 * 1000,
    controllable: true,
    ip: "192.168.1.31",
    resolution: "1920 × 1080",
    latency: 11,
    cpu: 32,
    mem: 58,
    gpu: "UHD 770",
    location: "客厅",
  },
  {
    id: "render-node",
    name: "工作室 · 渲染机",
    os: "Windows 11 Pro",
    version: "24H2",
    online: true,
    lastSeen: now - 12 * 1000,
    controllable: true,
    ip: "192.168.1.42",
    resolution: "3840 × 2160",
    latency: 24,
    cpu: 76,
    mem: 63,
    gpu: "RTX 4090",
    location: "工作室",
  },
  {
    id: "thinkpad",
    name: "随身 · ThinkPad",
    os: "Windows 10 Pro",
    version: "22H2",
    online: false,
    lastSeen: now - 2 * 60 * 60 * 1000,
    controllable: true,
    ip: "192.168.1.57",
    resolution: "1920 × 1200",
    latency: 0,
    cpu: 0,
    mem: 0,
    gpu: "Iris Xe",
    location: "随身",
  },
  {
    id: "backup",
    name: "备用机 · 老主机",
    os: "Windows 10",
    version: "21H2",
    online: false,
    lastSeen: now - 3 * 24 * 60 * 60 * 1000,
    controllable: false,
    ip: "192.168.1.66",
    resolution: "1680 × 1050",
    latency: 0,
    cpu: 0,
    mem: 0,
    gpu: "GTX 1060",
    location: "储物间",
  },
  // 演示「局域网直连」：api.connectDirect 的演示模式返回这个 deviceId，
  // 设备列表里要有对应项，控制页才能正常打开。
  {
    id: "direct-peer",
    name: "同网段 · 直连设备",
    os: "Windows 11 Pro",
    version: "24H2",
    online: true,
    lastSeen: now,
    controllable: true,
    ip: "192.168.1.77",
    resolution: "1920 × 1080",
    latency: 2,
    cpu: 12,
    mem: 38,
    gpu: "UHD 730",
    location: "同网段",
  },
];

/** 根据设备 id 稳定地挑选一套配色，避免每次渲染跳变。 */
export function deviceVariant(id: string): number {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  }
  return hash % 5;
}

export function formatLastSeen(ms: number): string {
  if (!ms) return "从未上线";
  const diff = Date.now() - ms;
  if (diff < 60 * 1000) return "刚刚";
  if (diff < 60 * 60 * 1000) return `${Math.floor(diff / 60000)} 分钟前`;
  if (diff < 24 * 60 * 60 * 1000) return `${Math.floor(diff / 3600000)} 小时前`;
  if (diff < 7 * 24 * 60 * 60 * 1000) return `${Math.floor(diff / 86400000)} 天前`;
  return new Date(ms).toLocaleDateString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function formatBytes(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${bytes} B`;
}

export function statusLabel(device: DeviceView): string {
  if (!device.online) return "离线";
  if (!device.controllable) return "仅查看";
  return "在线";
}
