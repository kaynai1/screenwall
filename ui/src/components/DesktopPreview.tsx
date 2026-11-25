import { Monitor } from "lucide-react";
import type { DeviceView } from "../types";
import "./DesktopPreview.css";

/**
 * 设备画面占位。没有真实帧时使用，不模拟桌面内容。
 */
export default function DesktopPreview({
  device,
  large = false,
  live = false,
  showName = true,
  frame,
  hint,
  self = false,
}: {
  device: DeviceView;
  large?: boolean;
  live?: boolean;
  showName?: boolean;
  /** 实时缩略图（JPEG data URL）；有帧时替换占位。 */
  frame?: string;
  /** 自定义占位文案，例如预览连接失败的原因。 */
  hint?: string;
  /** 本机：永远不推流，用浅色说明面代替深色视频底。 */
  self?: boolean;
}) {
  const offline = !device.online;
  const hasFrame = !offline && !self && Boolean(frame);

  return (
    <div
      className={[
        "desktop-preview",
        large ? "is-large" : "",
        offline ? "is-offline" : "",
        hasFrame ? "has-frame" : "",
        self ? "is-self" : "",
      ]
        .filter(Boolean)
        .join(" ")}
    >
      {!hasFrame && (
        <div className="dp-surface">
          <Monitor size={large ? 26 : 18} strokeWidth={1.5} aria-hidden="true" />
          <span className="dp-hint">
            {offline ? "设备离线" : (hint ?? (live ? "等待画面" : "暂无预览"))}
          </span>
        </div>
      )}

      {hasFrame && (
        <img className="dp-frame" src={frame} alt={`${device.name} 实时画面`} />
      )}

      {showName && (
        <span className="dp-name">
          {device.name} · {device.os}
        </span>
      )}

      {live && !offline && hasFrame && (
        <span className="dp-live">
          <i aria-hidden="true" />
          实时
        </span>
      )}
    </div>
  );
}
