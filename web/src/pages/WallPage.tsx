/**
 * 屏幕墙：每台在线设备各开一条 `wall` 只读会话（1 FPS / 480px），
 * 卡片里是实时缩略图，点一下进入完整控制。
 *
 * `wall` 会话由被控端免逐次授权、忽略输入与文件，只在页面打开期间存在；
 * 每张卡片用独立的 `Viewer`（独立 WebSocket），互不影响。
 */

import { ChevronLeft, Loader2, Monitor, WifiOff } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Viewer, type Account, type DeviceView } from "../api";
import { MEDIA_VIDEO } from "../protocol";
import { VideoRenderer } from "../video";
import { WEB_VERSION } from "../version";

type CardState = "connecting" | "live" | "error";

function WallCard({
  account,
  device,
  onOpen,
}: {
  account: Account;
  device: DeviceView;
  onOpen: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const [state, setState] = useState<CardState>("connecting");
  const [errorText, setErrorText] = useState("");

  useEffect(() => {
    const viewer = new Viewer(account);
    const renderer = new VideoRenderer();
    if (canvasRef.current) renderer.attach(canvasRef.current);
    let disposed = false;

    const fail = (text: string) => {
      if (disposed) return;
      setState("error");
      setErrorText(text);
    };

    const off = viewer.subscribe({
      onOpen: () => {
        viewer.sendJson({
          t: "hello",
          proto: 1,
          token: "",
          client: { name: "网页端", version: WEB_VERSION },
          caps: [],
        });
      },
      onMessage: (message) => {
        switch (message.t) {
          case "welcome":
            viewer.sendJson({
              t: "video.sub",
              stream: "wall",
              display: 0,
              fps: 1,
              maxWidth: 480,
              quality: 55,
              codec: "mjpeg",
              cursor: false,
            });
            break;
          case "video.sub.ok":
            setState("live");
            break;
          case "error":
            fail(String(message.msg ?? "被控端拒绝了预览"));
            break;
          default:
            break;
        }
      },
      onFrame: (frame) => {
        if (frame.type === MEDIA_VIDEO) renderer.push(frame);
      },
      onClosed: (_code, msg) => fail(msg),
      onSocketClosed: (reason) => fail(reason),
    });

    const run = async () => {
      try {
        await viewer.connect();
        await viewer.open(device.id, "wall");
      } catch (error) {
        fail(error instanceof Error ? error.message : String(error));
      }
    };
    void run();

    return () => {
      disposed = true;
      off();
      viewer.closeSession();
      viewer.close();
      renderer.detach();
    };
  }, [account, device.id]);

  return (
    <button type="button" className={`wall-card ${state}`} onClick={onOpen}>
      <div className="wall-thumb">
        <canvas ref={canvasRef} />
        {state !== "live" && (
          <div className="wall-placeholder">
            {state === "connecting" ? (
              <>
                <Loader2 size={18} className="spin" />
                <span>连接中…</span>
              </>
            ) : (
              <>
                <WifiOff size={18} />
                <span>{errorText || "暂时看不到画面"}</span>
              </>
            )}
          </div>
        )}
      </div>
      <div className="wall-copy">
        <strong>{device.name || "未命名设备"}</strong>
        <small>{state === "live" ? "实时预览 · 点击控制" : device.os}</small>
      </div>
    </button>
  );
}

export default function WallPage({
  account,
  devices,
  onOpen,
  onBack,
}: {
  account: Account;
  devices: DeviceView[];
  onOpen: (device: DeviceView) => void;
  onBack: () => void;
}) {
  const online = useMemo(
    () => devices.filter((device) => device.online && device.controllable),
    [devices],
  );

  return (
    <div className="page wall-page">
      <header className="app-bar">
        <button type="button" className="icon-btn" onClick={onBack} aria-label="返回">
          <ChevronLeft size={20} />
        </button>
        <div>
          <h1>屏幕墙</h1>
          <p className="app-bar-sub">{online.length} 台在线设备</p>
        </div>
      </header>

      {online.length === 0 ? (
        <div className="empty">
          <Monitor size={28} />
          <strong>没有可预览的设备</strong>
          <span>被控电脑在线、且开启了「允许被控」才会出现在这里。</span>
        </div>
      ) : (
        <div className="wall-grid">
          {online.map((device) => (
            <WallCard
              key={device.id}
              account={account}
              device={device}
              onOpen={() => onOpen(device)}
            />
          ))}
        </div>
      )}
    </div>
  );
}
