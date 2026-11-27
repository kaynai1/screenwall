import { useCallback, useEffect, useRef, useState } from "react";
import { AlertCircle, CheckCircle2, Info, Loader2 } from "lucide-react";
import {
  answerControlRequest,
  getAccount,
  getControlInfo,
  listControlRequests,
  listDevices,
  login as apiLogin,
  logout as apiLogout,
  quickConnect as apiQuickConnect,
  connectByLink as apiConnectByLink,
  enterLocalMode as apiEnterLocalMode,
  getDirectState,
  prewarmPanel,
  setDirectEnabled,
  subscribeAccount,
  subscribeConnectLink,
  subscribeControlRequest,
  subscribeControlState,
  subscribeDevices,
  takeConnectLink,
  type ConnectLinkInput,
  type ControlRequest,
} from "./api";
import ConnectLinkDialog from "./components/ConnectLinkDialog";
import ControlRequestDialog from "./components/ControlRequestDialog";
import { subscribeWatchdog } from "./connection";
import { describeError, errorLine } from "./errors";
import Sidebar from "./components/Sidebar";
import Topbar from "./components/Topbar";
import AuditPage from "./pages/AuditPage";
import ControlPage from "./pages/ControlPage";
import DevicesPage from "./pages/DevicesPage";
import FeedbackPage from "./pages/FeedbackPage";
import FilesPage from "./pages/FilesPage";
import GroupPage from "./pages/GroupPage";
import LoginPage from "./pages/LoginPage";
import ServerPage from "./pages/ServerPage";
import SettingsPage from "./pages/SettingsPage";
import WallPage from "./pages/WallPage";
import RemotePage from "./pages/RemotePage";
import { isDirectDeviceId, stopActiveSession } from "./session";
import { checkUpdate, currentVersion } from "./update";
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
  /** 收到但还没确认的连接链接（登录状态下需要先问是否退出账号）。 */
  const [pendingLink, setPendingLink] = useState<ConnectLinkInput | null>(null);
  /** 局域网直连开关状态（侧栏底部那个开关）。 */
  const [directOn, setDirectOn] = useState(false);
  const [directBusy, setDirectBusy] = useState(false);
  /** 当前客户端版本与启动时检查到的新版；左下角更新提示用。 */
  const [appVersion, setAppVersion] = useState("");
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  /** 当前远程会话真实走的链路；没有会话时为 null（侧栏胶囊按它显示）。 */
  const [controlLink, setControlLink] = useState<
    "relay" | "direct" | "p2p" | null
  >(null);
  /** 已经打开过的页面：切回来时复用，不再重新挂载、重新拉数据。 */
  const [visited, setVisited] = useState<Set<PageKey>>(() => new Set(["wall"]));
  // 任何方式切页（侧栏 navigate、屏幕墙/设备页 openDevice、登录后进控制页）都要
  // 把目标页登记进 visited：页面容器只渲染 visited 里的项，漏登记就会整页空白。
  // openDevice 等路径不走 navigate，所以这里统一兜住。
  useEffect(() => {
    setVisited((current) => {
      if (current.has(page)) return current;
      const copy = new Set(current);
      copy.add(page);
      return copy;
    });
  }, [page]);
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

  // 左下角更新提示：启动时取一次当前版本；登录（含局域网模式）后检查一次新版。
  // 检查失败就静默——设置页里还有手动「检查更新」，不该为网络问题弹提示。
  useEffect(() => {
    let alive = true;
    currentVersion()
      .then((value) => {
        if (alive) setAppVersion(value);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    if (!account?.loggedIn) return;
    let alive = true;
    checkUpdate()
      .then((info) => {
        if (alive && info) setUpdateVersion(info.version);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [account?.loggedIn, account?.local]);

  // 直连开关状态：只在局域网模式下有意义，但登录状态下也读一次，
  // 这样侧栏底部能反映真实状态而不是猜。
  useEffect(() => {
    if (!account?.loggedIn) return;
    let alive = true;
    const load = () => {
      void getDirectState()
        .then((next) => {
          if (alive) setDirectOn(next.enabled);
        })
        .catch(() => {});
    };
    load();
    const timer = window.setInterval(load, 5000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [account?.loggedIn, account?.local]);

  // 侧栏胶囊要按实际链路说话：进入时读一次当前会话，之后跟随 control-state。
  // 没有会话时保持 null，侧栏按默认路径显示「服务器中转」。
  useEffect(() => {
    if (!account?.loggedIn) {
      setControlLink(null);
      return;
    }
    let alive = true;
    const sync = () => {
      void getControlInfo()
        .then((info) => {
          if (alive) setControlLink(info ? (info.link ?? "relay") : null);
        })
        .catch(() => {});
    };
    sync();
    // 事件之外再定时对一次账：P2P/直连建立时的状态事件、页面保活导致的事件
    // 丢失，都可能让胶囊停在旧值上；胶囊显示的是「现在怎么连」，不能猜。
    const poll = window.setInterval(sync, 2500);
    const off = subscribeControlState((state) => {
      if (!alive) return;
      if (state.phase === "closed" || state.phase === "error") {
        setControlLink(null);
      } else if (state.link) {
        setControlLink(state.link);
      }
    });
    return () => {
      alive = false;
      window.clearInterval(poll);
      off();
    };
  }, [account?.loggedIn]);

  // 后台预启动服务器管理面板子进程：用户第一次点「服务器管理」时
  // 后端复用已启动进程直接返回握手，省掉首次 spawn + 握手的等待。
  useEffect(() => {
    if (!account?.loggedIn || account.quick) return;
    void prewarmPanel().catch(() => {});
  }, [account?.loggedIn, account?.quick]);

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

  /** 进入局域网模式：不连服务器，只走直连。 */
  async function handleLocalMode() {
    if (busy) return;
    setBusy(true);
    setLoginError(null);
    try {
      const next = await apiEnterLocalMode();
      setAccount(next);
      const list = await listDevices().catch(() => [] as DeviceView[]);
      setDevices(list);
      setPage("wall");
      notify("已进入局域网模式，不需要服务器", "success");
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

  /** 用一次性链接连入：成功后的界面状态与连接码连接完全一致。 */
  const runConnectLink = useCallback(
    async (link: ConnectLinkInput) => {
      setBusy(true);
      setLoginError(null);
      try {
        const next = await apiConnectByLink(link);
        setAccount(next);
        const list = await listDevices().catch(() => [] as DeviceView[]);
        setDevices(list);
        setActiveId(list[0]?.id ?? null);
        setPage("control");
        notify(list[0] ? `已连接 ${list[0].name}` : "已连接到远程设备", "success");
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
    },
    [notify],
  );

  // 连接链接：启动时主动取一次（协议唤起早于前端挂载），运行中再监听事件。
  const accountRef = useRef(account);
  accountRef.current = account;
  const runLinkRef = useRef(runConnectLink);
  runLinkRef.current = runConnectLink;

  useEffect(() => {
    let alive = true;
    const accept = (link: ConnectLinkInput) => {
      if (!alive) return;
      // 已登录时链接会顶掉账号会话：先让用户确认，不悄悄退出。
      if (accountRef.current?.loggedIn) {
        setPendingLink(link);
      } else {
        void runLinkRef.current(link);
      }
    };
    const off = subscribeConnectLink(accept);
    takeConnectLink()
      .then((link) => {
        if (link) accept(link);
      })
      .catch(() => {});
    return () => {
      alive = false;
      off();
    };
  }, []);

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

  function openDevice(id: string, fallback?: DeviceView) {
    // 直连设备用合成 ID（`direct:ip:port`），不在账号设备列表里，
    // 会话已经由 connectDirect 建好，控制页会复用它。
    //
    // `fallback`：设备刚被服务器按识别码解析出来。服务器已经确认它属于本账号
    // 且在线，但设备列表是服务端异步推送的，此刻本地列表里可能还没有它 ——
    // 先把它补进列表，否则控制页会把它当成「列表里查不到的设备」。
    const device = devices.find((item) => item.id === id);
    if (!device && !isDirectDeviceId(id)) {
      if (!fallback) {
        notify("设备不存在或已被移除", "error");
        return;
      }
      setDevices((current) =>
        current.some((item) => item.id === id) ? current : [...current, fallback],
      );
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
    setVisited((current) => {
      if (current.has(next)) return current;
      const copy = new Set(current);
      copy.add(next);
      return copy;
    });
  }

  function selectForGroup(ids: string[]) {
    setSelectedIds(ids);
    setPage("group");
  }

  /** 侧栏底部的直连开关。 */
  async function toggleDirect() {
    if (!account?.local) {
      notify("直连开关只在局域网模式下可用", "info");
      return;
    }
    setDirectBusy(true);
    try {
      const next = await setDirectEnabled(!directOn);
      setDirectOn(next.enabled);
      notify(next.enabled ? "已开启局域网直连" : "已关闭局域网直连", "success");
    } catch (error) {
      const { title, detail } = describeError(error);
      notify(`${title}：${detail}`, "error");
    } finally {
      setDirectBusy(false);
    }
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
        onLocalMode={handleLocalMode}
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

  function renderPage(target: PageKey = currentPage) {
    // 保活的页面里只有当前页是活跃的：轮询/心跳隐藏时停掉，回显时恢复。
    const active = target === currentPage;
    switch (target) {
      case "remote":
        return <RemotePage {...common} active={active} onOpenDevice={openDevice} />;
      case "wall":
        return (
          <WallPage
            {...common}
            active={active}
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
            active={active}
            selectedIds={selectedIds}
            setSelectedIds={setSelectedIds}
            onOpenDevice={openDevice}
          />
        );
      case "files":
        return (
          <FilesPage
            {...common}
            active={active}
            initialDeviceId={activeId}
            onOpenWall={() => navigate("wall")}
          />
        );
      case "devices":
        return <DevicesPage {...common} active={active} onOpenDevice={openDevice} />;
      case "audit":
        return <AuditPage {...common} active={active} />;
      case "server":
        return <ServerPage active={active} />;
      case "feedback":
        return <FeedbackPage {...common} active={active} />;
      case "settings":
        return (
          <SettingsPage
            {...common}
            active={active}
            onLogout={handleLogout}
            busy={busy}
          />
        );
      case "control":
        return controlDeviceId ? (
          <ControlPage
            {...common}
            active={active}
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
        onOpenSettings={() => navigate("settings")}
        directOn={directOn}
        directBusy={directBusy}
        onToggleDirect={() => void toggleDirect()}
        link={controlLink}
        appVersion={appVersion}
        updateVersion={updateVersion}
      />

      <div className="app-main">
        <Topbar page={currentPage} />
        <div
          className={`page-shell ${
            currentPage === "control" || currentPage === "server" ? "is-flush" : ""
          }`}
        >
          {/* 访问过的页面保留在 DOM 里，切回来只切 display。
              之前每次切页都换 key，整页卸载重挂、每个页面的 useEffect
              又各自 invoke 后端，所以点一下要等好几秒才有反应。 */}
          {Array.from(
            // 当前页永远在渲染集合里：即使某条切页路径还没来得及登记 visited，
            // 也不会出现「顶栏说在控制页、内容区空白」的一帧。
            visited.has(currentPage) ? visited : new Set(visited).add(currentPage),
          ).map((key) => (
            <div
              className={`page-enter ${key === currentPage ? "" : "is-hidden"}`}
              key={key}
              aria-hidden={key === currentPage ? undefined : true}
            >
              {renderPage(key)}
            </div>
          ))}
        </div>
      </div>

      <ControlRequestDialog
        request={requests[0] ?? null}
        onAnswer={answerRequest}
      />

      <ConnectLinkDialog
        link={pendingLink}
        busy={busy}
        onCancel={() => setPendingLink(null)}
        onConfirm={() => {
          const link = pendingLink;
          setPendingLink(null);
          if (!link) return;
          void (async () => {
            // 先退出当前账号：链接会话是匿名的，不能两个会话并存。
            await stopActiveSession().catch(() => {});
            await apiLogout().catch(() => {});
            await runConnectLink(link);
          })();
        }}
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
