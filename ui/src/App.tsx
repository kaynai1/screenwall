import { useCallback, useEffect, useRef, useState } from "react";
import { AlertCircle, CheckCircle2, Info, Loader2 } from "lucide-react";
import {
  answerControlRequest,
  getAccount,
  listControlRequests,
  listDevices,
  login as apiLogin,
  logout as apiLogout,
  quickConnect as apiQuickConnect,
  subscribeAccount,
  subscribeControlRequest,
  subscribeDevices,
  type ControlRequest,
} from "./api";
import ControlRequestDialog from "./components/ControlRequestDialog";
import { subscribeWatchdog } from "./connection";
import { describeError, errorLine } from "./errors";
import Sidebar from "./components/Sidebar";
import Topbar from "./components/Topbar";
import AuditPage from "./pages/AuditPage";
import ControlPage from "./pages/ControlPage";
import DevicesPage from "./pages/DevicesPage";
import FilesPage from "./pages/FilesPage";
import GroupPage from "./pages/GroupPage";
import LoginPage from "./pages/LoginPage";
import ServerPage from "./pages/ServerPage";
import SettingsPage from "./pages/SettingsPage";
import WallPage from "./pages/WallPage";
import { isDirectDeviceId, stopActiveSession } from "./session";
import type {
  AccountView,
  DeviceView,
  PageKey,
  ToastMessage,
  ToastTone,
} from "./types";

const EMPTY_ACCOUNT: AccountView = {
  serverUrl: "http://127.0.0.1:8787",
  username: "",
  deviceId: "",
  loggedIn: false,
  connected: false,
  error: null,
};

export default function App() {
  const [account, setAccount] = useState<AccountView | null>(null);
  const [devices, setDevices] = useState<DeviceView[]>([]);
  const [booted, setBooted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [loginError, setLoginError] = useState<string | null>(null);
  const [bootError, setBootError] = useState<string | null>(null);
  const [page, setPage] = useState<PageKey>("wall");
  const [activeId, setActiveId] = useState<string | null>(null);
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [toasts, setToasts] = useState<ToastMessage[]>([]);
  const [requests, setRequests] = useState<ControlRequest[]>([]);
  const toastSeq = useRef(0);

  const notify = useCallback((text: string, tone: ToastTone = "info") => {
    const id = toastSeq.current + 1;
    toastSeq.current = id;
    setToasts((current) => [...current, { id, text, tone }]);
    window.setTimeout(() => {
      setToasts((current) => current.filter((toast) => toast.id !== id));
    }, 3600);
  }, []);

  useEffect(() => {
    let alive = true;
    const offAccount = subscribeAccount((next) => {
      if (alive) setAccount(next);
    });
    const offDevices = subscribeDevices((next) => {
      if (alive) setDevices(next);
    });

    (async () => {
      try {
        const [nextAccount, nextDevices] = await Promise.all([
          getAccount(),
          listDevices(),
        ]);
        if (!alive) return;
        setAccount(nextAccount);
        setDevices(nextDevices);
      } catch (error) {
        if (!alive) return;
        const { title, detail } = describeError(error);
        setBootError(`${title}：${detail}`);
        setAccount({ ...EMPTY_ACCOUNT, error: `${title}：${detail}` });
      } finally {
        if (alive) setBooted(true);
      }
    })();

    return () => {
      alive = false;
      offAccount();
      offDevices();
    };
  }, []);

  // 被控端授权：事件 + 启动时补拉，避免窗口挂载前的事件丢失。
  useEffect(() => {
    let alive = true;
    const push = (next: ControlRequest) => {
      setRequests((current) =>
        current.some((item) => item.session === next.session)
          ? current
          : [...current, next],
      );
    };
    const off = subscribeControlRequest((next) => {
      if (alive) push(next);
    });
    listControlRequests()
      .then((list) => {
        if (alive) list.forEach(push);
      })
      .catch(() => {});
    return () => {
      alive = false;
      off();
    };
  }, []);

  // 看门狗重启会话任务时给用户一个明确反馈，避免界面静默停在「连接中」。
  useEffect(() => {
    return subscribeWatchdog(() => {
      notify("连接中断较久，正在自动重连…", "info");
    });
  }, [notify]);

  const answerRequest = useCallback(
    async (session: number, allow: boolean, reason?: "timeout") => {
      setRequests((current) =>
        current.filter((item) => item.session !== session),
      );
      const target = requests.find((item) => item.session === session);
      try {
        await answerControlRequest(session, allow);
      } catch {
        notify("该控制请求已失效", "info");
        return;
      }
      if (!allow) {
        notify(
          reason === "timeout" ? "控制请求超时，已自动拒绝" : "已拒绝远程控制请求",
          "info",
        );
      } else {
        notify(
          target ? `已允许 ${target.viewer} 控制本机` : "已允许远程控制",
          "success",
        );
      }
    },
    [notify, requests],
  );

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && page === "control") {
        navigate("wall");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [page]);

  async function handleLogin(input: {
    serverUrl: string;
    username: string;
    password: string;
    register: boolean;
  }) {
    if (busy) return;
    setBusy(true);
    setLoginError(null);
    try {
      const next = await apiLogin(input);
      setAccount(next);
      const list = await listDevices().catch(() => devices);
      setDevices(list);
      setPage("wall");
      notify(`已登录 ${next.username}`, "success");

      // login 命令返回时，主控 WebSocket 可能还在建连；
      // 稍后再取一次账号视图，避免顶栏一直停在「正在连接」。
      window.setTimeout(() => {
        Promise.all([getAccount(), listDevices()])
          .then(([fresh, freshDevices]) => {
            setAccount(fresh);
            setDevices(freshDevices);
          })
          .catch(() => {});
      }, 1200);
    } catch (error) {
      const { title, detail } = describeError(error);
      setLoginError(`${title}：${detail}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleQuickConnect(input: {
    serverUrl: string;
    code: string;
    password: string;
  }) {
    if (busy) return;
    setBusy(true);
    setLoginError(null);
    try {
      const next = await apiQuickConnect(input);
      setAccount(next);
      const list = await listDevices().catch(() => [] as DeviceView[]);
      setDevices(list);
      const target = list[0] ?? null;
      setActiveId(target?.id ?? null);
      setPage("control");
      notify(
        target ? `已连接 ${target.name}` : "已连接到远程设备",
        "success",
      );

      // 主控 WebSocket 可能还在建连，稍后再刷新一次状态。
      window.setTimeout(() => {
        Promise.all([getAccount(), listDevices()])
          .then(([fresh, freshDevices]) => {
            setAccount(fresh);
            setDevices(freshDevices);
            if (freshDevices[0]) {
              setActiveId((current) => current ?? freshDevices[0].id);
            }
          })
          .catch(() => {});
      }, 1200);
    } catch (error) {
      const { title, detail } = describeError(error);
      setLoginError(`${title}：${detail}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleLogout() {
    if (busy) return;
    setBusy(true);
    try {
      await stopActiveSession();
      const next = await apiLogout();
      setAccount(next);
      setDevices([]);
      setPage("wall");
      setActiveId(null);
      setSelectedIds([]);
    } catch (error) {
      notify(errorLine(error), "error");
    } finally {
      setBusy(false);
    }
  }

  function openDevice(id: string) {
    // 直连设备用合成 ID（`direct:ip:port`），不在账号设备列表里，
    // 会话已经由 connectDirect 建好，控制页会复用它。
    const device = devices.find((item) => item.id === id);
    if (!device && !isDirectDeviceId(id)) {
      notify("设备不存在或已被移除", "error");
      return;
    }
    setActiveId(id);
    setPage("control");
  }

  function toggleSelect(id: string) {
    setSelectedIds((current) =>
      current.includes(id)
        ? current.filter((item) => item !== id)
        : [...current, id],
    );
  }

  function navigate(next: PageKey) {
    if (next === page) return;
    // 控制页和文件页共用同一条远程会话：只有「控制 → 文件」保持连接，
    // 其他跳转都断开，避免画面在后台一直跑。
    const keepSession = page === "control" && next === "files";
    if (!keepSession && (page === "control" || page === "files")) {
      void stopActiveSession();
    }
    setPage(next);
  }

  function selectForGroup(ids: string[]) {
    setSelectedIds(ids);
    setPage("group");
  }

  if (!booted) {
    return (
      <div className="splash-screen">
        <div className="splash-mark">
          <div className="brand-mark" aria-hidden="true">
            <span />
            <span />
            <span />
          </div>
          <Loader2 size={18} className="spin" />
        </div>
        <p>正在连接 ScreenWall…</p>
      </div>
    );
  }

  if (!account || !account.loggedIn) {
    return (
      <LoginPage
        account={account ?? EMPTY_ACCOUNT}
        busy={busy}
        error={loginError ?? bootError ?? account?.error ?? null}
        onLogin={handleLogin}
        onQuickConnect={handleQuickConnect}
      />
    );
  }

  const common = { account, devices, notify };
  const quick = Boolean(account.quick);
  const activeDevice =
    devices.find((device) => device.id === activeId) ?? null;
  // 直连设备的合成 ID 不在设备列表里，但控制页能复用已有会话，不能回退到屏幕墙。
  const activeKnown =
    activeDevice !== null || (activeId !== null && isDirectDeviceId(activeId));
  // 连接码会话只有一台设备，也没有屏幕墙/设备页可去：除设置外都停在控制页。
  const currentPage = quick
    ? page === "settings"
      ? "settings"
      : "control"
    : page === "control" && !activeKnown
      ? "wall"
      : page;
  const controlDeviceId = quick
    ? (activeId ?? devices[0]?.id ?? null)
    : activeKnown
      ? activeId
      : null;

  function renderPage() {
    switch (currentPage) {
      case "wall":
        return (
          <WallPage
            {...common}
            selectedIds={selectedIds}
            onToggleSelect={toggleSelect}
            onClearSelection={() => setSelectedIds([])}
            onOpenDevice={openDevice}
            onGroup={() => selectForGroup(selectedIds)}
            onFiles={(deviceId) => {
              if (deviceId) setActiveId(deviceId);
              navigate("files");
            }}
            onDevices={() => navigate("devices")}
          />
        );
      case "group":
        return (
          <GroupPage
            {...common}
            selectedIds={selectedIds}
            setSelectedIds={setSelectedIds}
            onOpenDevice={openDevice}
          />
        );
      case "files":
        return (
          <FilesPage
            {...common}
            initialDeviceId={activeId}
            onOpenWall={() => navigate("wall")}
          />
        );
      case "devices":
        return <DevicesPage {...common} onOpenDevice={openDevice} />;
      case "audit":
        return <AuditPage {...common} />;
      case "server":
        return <ServerPage />;
      case "settings":
        return (
          <SettingsPage
            {...common}
            onLogout={handleLogout}
            busy={busy}
          />
        );
      case "control":
        return controlDeviceId ? (
          <ControlPage
            {...common}
            deviceId={controlDeviceId}
            onBack={() => {
              if (quick) {
                void handleLogout();
              } else {
                navigate("wall");
              }
            }}
            onFiles={() => {
              if (!quick) navigate("files");
            }}
            onOpenDevice={openDevice}
          />
        ) : null;
      default:
        return null;
    }
  }

  return (
    <div className="app-shell">
      <Sidebar
        page={currentPage}
        onNavigate={navigate}
        account={account}
        devices={devices}
        selectedCount={selectedIds.length}
        onLogout={handleLogout}
      />

      <div className="app-main">
        <Topbar
          page={currentPage}
          account={account}
          onOpenSettings={() => navigate("settings")}
        />
        <div
          className={`page-shell ${
            currentPage === "control" || currentPage === "server" ? "is-flush" : ""
          }`}
        >
          <div className="page-enter" key={`${currentPage}:${activeId ?? ""}`}>
            {renderPage()}
          </div>
        </div>
      </div>

      <ControlRequestDialog
        request={requests[0] ?? null}
        onAnswer={answerRequest}
      />

      <div className="toast-stack" aria-live="polite">
        {toasts.map((toast) => (
          <div className={`toast toast-${toast.tone}`} key={toast.id}>
            {toast.tone === "success" ? (
              <CheckCircle2 size={16} />
            ) : toast.tone === "error" ? (
              <AlertCircle size={16} />
            ) : (
              <Info size={16} />
            )}
            <span>{toast.text}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
