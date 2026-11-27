import type { ComponentType } from "react";
import {
  FolderSync,
  History,
  LayoutGrid,
  Link2,
  MonitorSmartphone,
  Server,
  Settings2,
  UsersRound,
} from "lucide-react";
import { BugIcon } from "./components/QingIcons";
import type { PageKey } from "./types";

/**
 * 导航图标的最小接口：Lucide 与 QingIcon 内联组件都满足它
 * （Lucide 的图标类型额外带一堆可选属性，直接当 LucideIcon 用会把 QingIcon 排除掉）。
 */
export type NavIcon = ComponentType<{
  size?: number | string;
  strokeWidth?: number | string;
  className?: string;
}>;

export type NavItem = {
  key: Exclude<PageKey, "control">;
  label: string;
  icon: NavIcon;
};

export const NAV_SECTIONS: { label: string; items: NavItem[] }[] = [
  {
    label: "工作台",
    items: [
      { key: "remote", label: "远程协助", icon: Link2 },
      { key: "wall", label: "屏幕墙", icon: LayoutGrid },
      { key: "group", label: "群控", icon: UsersRound },
      { key: "files", label: "文件传输", icon: FolderSync },
    ],
  },
  {
    label: "管理",
    items: [
      { key: "devices", label: "设备", icon: MonitorSmartphone },
      { key: "audit", label: "审计", icon: History },
      { key: "server", label: "服务器管理", icon: Server },
      { key: "feedback", label: "建议与 BUG", icon: BugIcon },
      { key: "settings", label: "设置", icon: Settings2 },
    ],
  },
];

export const PAGE_META: Record<PageKey, { title: string; sub: string }> = {
  remote: { title: "远程协助", sub: "识别码与密码，一页搞定" },
  wall: { title: "屏幕墙", sub: "所有设备，一眼掌握" },
  group: { title: "群控", sub: "一次操作，多台同步" },
  files: { title: "文件传输", sub: "在设备之间拖拽文件" },
  devices: { title: "设备管理", sub: "设备、权限与在线状态" },
  audit: { title: "会话审计", sub: "谁在什么时候连过哪台设备" },
  server: { title: "服务器管理", sub: "通过 SSH 管理 Linux 主机" },
  feedback: { title: "建议与 BUG", sub: "你的反馈会直接送到开发者手上" },
  settings: { title: "设置", sub: "账号、安全与使用偏好" },
  control: { title: "远程控制", sub: "低延迟画面与键鼠直通" },
};
