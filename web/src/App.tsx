import { useCallback, useEffect, useRef, useState } from "react";
import { logout as apiLogout, Viewer, type Account, type DeviceView } from "./api";
import ControlPage from "./pages/ControlPage";
import DevicesPage from "./pages/DevicesPage";
import GroupPage from "./pages/GroupPage";
import LoginPage from "./pages/LoginPage";
import RemotePage from "./pages/RemotePage";
import WallPage from "./pages/WallPage";

const STORAGE_KEY = "screenwall.web.account";

function loadAccount(): Account | null {
  try {
    const text = localStorage.getItem(STORAGE_KEY);
    if (!text) return null;
    const value = JSON.parse(text) as Account;
    if (!value?.token || !value?.serverUrl) return null;
    if (value.expiresAt && value.expiresAt < Date.now()) return null;
    return value;
  } catch {
    return null;
  }
}

function saveAccount(account: Account | null): void {
  try {
    if (account) localStorage.setItem(STORAGE_KEY, JSON.stringify(account));
    else localStorage.removeItem(STORAGE_KEY);
  } catch {
    // 隐私模式下 localStorage 可能不可用，忽略。
  }
}

export type ToastMessage = { id: number; text: string };

export default function App() {
  const [account, setAccount] = useState<Account | null>(loadAccount);
  const [devices, setDevices] = useState<DeviceView[]>([]);
  const [connected, setConnected] = useState(false);
  const [connectionError, setConnectionError] = useState<string | null>(null);
  const [target, setTarget] = useState<DeviceView | null>(null);
  const [wallOpen, setWallOpen] = useState(false);
  const [groupOpen, setGroupOpen] = useState(false);
  const [remoteOpen, setRemoteOpen] = useState(false);
  const [toast, setToast] = useState<ToastMessage | null>(null);
  const viewerRef = useRef<Viewer | null>(null);
  const toastSeq = useRef(0);

  const notify = useCallback((text: string) => {
    toastSeq.current += 1;
    const id = toastSeq.current;
    setToast({ id, text });
    window.setTimeout(() => {
      setToast((current) => (current?.id === id ? null : current));
    }, 2800);
  }, []);

  // 账号变化 → 建一条到服务器的长连接（设备列表、会话都走它）。
  useEffect(() => {
    if (!account) {
      viewerRef.current = null;
      setDevices([]);
      setConnected(false);
      setTarget(null);
      return;
    }
    const viewer = new Viewer(account);
    viewerRef.current = viewer;
    let alive = true;
    const offDevices = viewer.subscribe({
      // 网页登录本身会注册一台「网页端」设备，它永远不可能被控，别列出来；
      // 老版本每次登录换一个 ID 留下的历史「网页端」也一并隐藏。
      onDevices: (list) =>
        setDevices(
          list.filter(
            (item) => item.id !== account.deviceId && item.os.toLowerCase() !== "web",
          ),
        ),
    });
    const offSocket = viewer.subscribe({
      onSocketClosed: (reason) => {
        if (!alive) return;
        setConnected(false);
        setConnectionError(reason);
      },
    });
    setConnectionError(null);
    viewer
      .connect()
      .then(() => {
        if (alive) {
          setConnected(true);
          setConnectionError(null);
        }
      })
      .catch((error: unknown) => {
        if (!alive) return;
        setConnectionError(error instanceof Error ? error.message : "无法连接服务器");
      });
    return () => {
      alive = false;
      offDevices();
      offSocket();
      viewer.close();
      viewerRef.current = null;
    };
  }, [account]);

  // 连接码会话只有一台设备，连上后直接进控制页。
  useEffect(() => {
    if (!account?.quick || target || !connected) return;
    const device = devices[0];
    if (device?.online && device.controllable) setTarget(device);
  }, [account, devices, target, connected]);

  const handleLogout = useCallback(async () => {
    const current = account;
    viewerRef.current?.close();
    viewerRef.current = null;
    saveAccount(null);
    setAccount(null);
    setTarget(null);
    setWallOpen(false);
    setGroupOpen(false);
    setRemoteOpen(false);
    if (current) void apiLogout(current);
  }, [account]);

  const handleAccount = useCallback((next: Account) => {
    saveAccount(next);
    setAccount(next);
  }, []);

  if (!account) {
    return <LoginPage onAccount={handleAccount} />;
  }

  const viewer = viewerRef.current;

  return (
    <>
      {target && viewer && connected ? (
        <ControlPage
          viewer={viewer}
          device={target}
          onBack={() => setTarget(null)}
          notify={notify}
        />
      ) : wallOpen && connected ? (
        <WallPage
          account={account}
          devices={devices}
          onOpen={(device) => {
            setWallOpen(false);
            setTarget(device);
          }}
          onBack={() => setWallOpen(false)}
        />
      ) : groupOpen && connected ? (
        <GroupPage account={account} devices={devices} onBack={() => setGroupOpen(false)} />
      ) : remoteOpen && connected ? (
        <RemotePage
          account={account}
          onBack={() => setRemoteOpen(false)}
          onOpenDevice={(device) => setTarget(device)}
          notify={notify}
        />
      ) : (
        <DevicesPage
          account={account}
          devices={devices}
          connected={connected}
          connectionError={connectionError}
          onOpen={(device) => setTarget(device)}
          onOpenWall={() => setWallOpen(true)}
          onOpenGroup={() => setGroupOpen(true)}
          onOpenRemote={() => setRemoteOpen(true)}
          onLogout={handleLogout}
          onReconnect={() => setAccount({ ...account })}
        />
      )}

      {toast && (
        <div className="toast" role="status" key={toast.id}>
          {toast.text}
        </div>
      )}
    </>
  );
}
