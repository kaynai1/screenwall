import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import {
  Cable,
  Check,
  Copy,
  FolderOpen,
  KeyRound,
  LogIn,
  MonitorPlay,
  MonitorSmartphone,
  MoreHorizontal,
  PencilLine,
  Plus,
  Power,
  RefreshCw,
  Search,
  ShieldAlert,
  Trash2,
  Wifi,
} from "lucide-react";
import { EmptyState, Modal, Segmented, Switch } from "../components/ui";
import {
  connectDirect,
  deleteDevice,
  refreshDevices as fetchDevices,
  renameDevice,
  scanDirectDevices,
  setDeviceControllable,
  wakeOnLan,
  wakeViaDevice,
  type DiscoveredDevice,
} from "../api";
import { errorLine } from "../errors";
import { formatLastSeen } from "../mock";
import type { DeviceView, DevicesPageProps } from "../types";
import "./DevicesPage.css";

type DeviceFilter = "all" | "online" | "offline";

/** 直连默认端口，与 docs/direct.md 的 47900 一致。 */
const DIRECT_PORT = 47900;

/**
 * 把「IP」或「IP:端口」拆开；端口缺省用 47900。
 * 返回 null 表示地址格式不对。
 */
function parseDirectAddress(
  raw: string,
): { host: string; port?: number } | null {
  const value = raw.trim();
  if (!value) return null;
  const sep = value.lastIndexOf(":");
  if (sep > 0 && sep < value.length - 1 && /^\d+$/.test(value.slice(sep + 1))) {
    const host = value.slice(0, sep).trim();
    const port = Number(value.slice(sep + 1));
    if (!host || !Number.isInteger(port) || port < 1 || port > 65535) {
      return null;
    }
    return { host, port: port === DIRECT_PORT ? undefined : port };
  }
  return { host: value };
}

const FILTER_OPTIONS: { value: DeviceFilter; label: string }[] = [
  { value: "all", label: "全部" },
  { value: "online", label: "在线" },
  { value: "offline", label: "离线" },
];

export default function DevicesPage({
  account,
  devices,
  notify,
  onOpenDevice,
}: DevicesPageProps) {
  const onlineCount = devices.filter((device) => device.online).length;

  const [controlState, setControlState] = useState<Record<string, boolean>>(
    () => {
      const initial: Record<string, boolean> = {};
      for (const device of devices) initial[device.id] = device.controllable;
      return initial;
    },
  );
  const [addOpen, setAddOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [renameTarget, setRenameTarget] = useState<DeviceView | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [renameBusy, setRenameBusy] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<DeviceView | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [controlBusy, setControlBusy] = useState<Record<string, boolean>>({});
  // 远程开机：被控端离线时没有会话，只能由本机向它的网段广播 magic packet。
  const [wakeTarget, setWakeTarget] = useState<DeviceView | null>(null);
  const [wakeMac, setWakeMac] = useState("");
  const [wakeBroadcast, setWakeBroadcast] = useState("255.255.255.255");
  // 空串表示由本机网卡广播；否则是代为发包的在线设备 ID。
  const [wakeHelper, setWakeHelper] = useState("");
  const [wakeBusy, setWakeBusy] = useState(false);

  // 能代为发包的设备：在线、允许被控，且不是本机或唤醒目标本身。
  const wakeHelpers = useMemo(
    () =>
      devices.filter(
        (device) =>
          device.online &&
          device.id !== account.deviceId &&
          device.id !== wakeTarget?.id &&
          (controlState[device.id] ?? device.controllable),
      ),
    [devices, account.deviceId, wakeTarget, controlState],
  );

  // 局域网直连：对方地址 + 可选配对码，连上后直接进控制页。
  const [directOpen, setDirectOpen] = useState(false);
  const [directHost, setDirectHost] = useState("");
  const [directCode, setDirectCode] = useState("");
  const [directBusy, setDirectBusy] = useState(false);
  const [directError, setDirectError] = useState<string | null>(null);
  // 局域网扫描：UDP 广播发现的同网段设备。
  const [scanBusy, setScanBusy] = useState(false);
  const [scanList, setScanList] = useState<DiscoveredDevice[] | null>(null);

  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<DeviceFilter>("all");
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const refreshTimer = useRef<number | null>(null);
  const copiedTimer = useRef<number | null>(null);

  useEffect(() => {
    setControlState((current) => {
      let next: Record<string, boolean> | null = null;
      for (const device of devices) {
        if (current[device.id] === undefined) {
          if (!next) next = { ...current };
          next[device.id] = device.controllable;
        }
      }
      return next ?? current;
    });
  }, [devices]);

  useEffect(() => {
    return () => {
      if (refreshTimer.current !== null) {
        window.clearTimeout(refreshTimer.current);
      }
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
      }
    };
  }, []);

  const filteredDevices = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return devices.filter((device) => {
      if (filter === "online" && !device.online) return false;
      if (filter === "offline" && device.online) return false;
      if (!needle) return true;
      return [
        device.name,
        device.os,
        device.version,
        device.ip ?? "",
        device.id,
      ]
        .join(" ")
        .toLowerCase()
        .includes(needle);
    });
  }, [devices, filter, query]);

  const filterActive = filter !== "all" || query.trim().length > 0;

  function displayName(device: DeviceView): string {
    return device.name;
  }

  /** 设备上次上报的 IP 是 A.B.C.D，就猜 A.B.C.255 作为定向广播地址。 */
  function suggestBroadcast(ip: string | null | undefined): string {
    if (!ip) return "255.255.255.255";
    const parts = ip.split(".");
    if (parts.length !== 4) return "255.255.255.255";
    return `${parts[0]}.${parts[1]}.${parts[2]}.255`;
  }

  async function refreshDevices() {
    if (refreshing) return;
    setRefreshing(true);
    try {
      const list = await fetchDevices();
      if (refreshTimer.current !== null) {
        window.clearTimeout(refreshTimer.current);
      }
      refreshTimer.current = window.setTimeout(() => {
        setRefreshing(false);
        refreshTimer.current = null;
        notify(
          `设备列表已刷新，${list.filter((device) => device.online).length} 台设备在线`,
          "success",
        );
      }, 500);
    } catch (error) {
      setRefreshing(false);
      notify(
        `刷新失败：${errorLine(error)}`,
        "error",
      );
    }
  }

  async function toggleControl(device: DeviceView, value: boolean) {
    if (controlBusy[device.id]) return;
    setControlBusy((current) => ({ ...current, [device.id]: true }));
    setControlState((current) => ({ ...current, [device.id]: value }));
    try {
      await setDeviceControllable(device.id, value);
      notify(
        `「${displayName(device)}」已设为${value ? "可控制" : "仅查看"}`,
        "success",
      );
    } catch (error) {
      setControlState((current) => ({ ...current, [device.id]: !value }));
      notify(
        `修改失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setControlBusy((current) => {
        const next = { ...current };
        delete next[device.id];
        return next;
      });
    }
  }

  function controlDevice(device: DeviceView) {
    if (!device.online) {
      notify(`「${displayName(device)}」当前离线，无法发起控制`, "error");
      return;
    }
    onOpenDevice(device.id);
  }

  function openAdd() {
    setCopied(false);
    setAddOpen(true);
  }

  function openRename(device: DeviceView) {
    setRenameTarget(device);
    setRenameDraft(displayName(device));
  }

  async function submitRename() {
    if (!renameTarget || renameBusy) return;
    const next = renameDraft.trim();
    if (!next) {
      notify("设备名称不能为空", "error");
      return;
    }
    if (next === renameTarget.name) {
      setRenameTarget(null);
      return;
    }
    setRenameBusy(true);
    try {
      await renameDevice(renameTarget.id, next);
      notify(`设备已重命名为「${next}」`, "success");
      setRenameTarget(null);
    } catch (error) {
      notify(
        `重命名失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setRenameBusy(false);
    }
  }

  async function confirmDelete() {
    if (!deleteTarget || deleteBusy) return;
    const name = displayName(deleteTarget);
    setDeleteBusy(true);
    try {
      await deleteDevice(deleteTarget.id);
      notify(`已删除设备「${name}」`, "success");
      setDeleteTarget(null);
    } catch (error) {
      notify(
        `删除失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setDeleteBusy(false);
    }
  }

  async function copyDeviceId() {
    const value = account.deviceId;
    if (!value) return;
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      notify("本机设备 ID 已复制", "success");
      if (copiedTimer.current !== null) {
        window.clearTimeout(copiedTimer.current);
      }
      copiedTimer.current = window.setTimeout(() => {
        setCopied(false);
        copiedTimer.current = null;
      }, 2000);
    } catch {
      notify("复制失败，请手动选择设备 ID", "error");
    }
  }

  function openWake(device: DeviceView) {
    setWakeTarget(device);
    setWakeMac("");
    setWakeBroadcast(suggestBroadcast(device.ip));
    setWakeHelper("");
  }

  async function submitWake() {
    const mac = wakeMac.trim();
    if (!/^([0-9a-f]{2}[:-]){5}[0-9a-f]{2}$|^[0-9a-f]{12}$/i.test(mac)) {
      notify("MAC 地址格式应为 AA:BB:CC:DD:EE:FF", "error");
      return;
    }
    const broadcast = wakeBroadcast.trim() || undefined;
    const helper = wakeHelpers.find((device) => device.id === wakeHelper);
    setWakeBusy(true);
    try {
      if (helper) {
        await wakeViaDevice(helper.id, mac, broadcast);
        notify(
          `已请「${displayName(helper)}」代发唤醒包；目标在同一局域网时，几秒后会开机`,
          "success",
        );
      } else {
        await wakeOnLan(mac, broadcast);
        notify(
          `已向 ${broadcast ?? "255.255.255.255"} 广播唤醒包；若目标在同一局域网，几秒后会开机`,
          "success",
        );
      }
      setWakeTarget(null);
    } catch (error) {
      notify(
        `${helper ? "代发唤醒包" : "发送唤醒包"}失败：${errorLine(error)}`,
        "error",
      );
    } finally {
      setWakeBusy(false);
    }
  }

  function openDirect() {
    setDirectHost("");
    setDirectCode("");
    setDirectError(null);
    setScanList(null);
    setDirectOpen(true);
  }

  /** 广播扫描同网段开启了直连的设备。 */
  async function scanLan() {
    if (scanBusy) return;
    setScanBusy(true);
    setDirectError(null);
    try {
      const found = await scanDirectDevices();
      setScanList(found);
      // 只找到一台就直接填好地址，省一次点击。
      if (found.length === 1) setDirectHost(found[0].address);
    } catch (error) {
      setDirectError(errorLine(error));
    } finally {
      setScanBusy(false);
    }
  }

  async function submitDirect() {
    if (directBusy) return;
    const address = parseDirectAddress(directHost);
    if (!address) {
      setDirectError("对方地址格式应为 192.168.1.20 或 192.168.1.20:47900");
      return;
    }
    if (directCode.length > 0 && directCode.length !== 6) {
      setDirectError("配对码是 6 位数字");
      return;
    }
    setDirectBusy(true);
    setDirectError(null);
    try {
      // 配对码可选：已配对过的设备不需要再输。
      const info = await connectDirect({
        host: address.host,
        port: address.port,
        pairingCode: directCode || undefined,
      });
      setDirectOpen(false);
      notify(`已直连 ${address.host}`, "success");
      // 与设备列表的「控制」按钮走同一条跳转路径。
      // 直连设备的 ID 是合成值（`direct:ip:port`），不在账号设备列表里；
      // App 的 openDevice 对直连 ID 放行，控制页会复用 connect_direct 建好的会话。
      onOpenDevice(info.deviceId);
    } catch (error) {
      setDirectError(errorLine(error));
    } finally {
      setDirectBusy(false);
    }
  }

  function clearFilters() {
    setQuery("");
    setFilter("all");
  }

  function openFiles(device: DeviceView) {
    if (!device.online) {
      notify(`「${displayName(device)}」当前离线，无法传输文件`, "error");
      return;
    }
    notify("请进入远程控制后使用文件传输", "info");
  }

  return (
    <div className="dev-page">
      <div className="dev-toolbar">
        <label className="field-control dev-search">
          <Search size={15} />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索设备名称、系统或 IP"
            aria-label="搜索设备"
          />
        </label>

        <Segmented<DeviceFilter>
          value={filter}
          options={FILTER_OPTIONS}
          onChange={setFilter}
        />

        <span className="dev-meta">
          共 {devices.length} 台 · {onlineCount} 台在线
        </span>

        <div className="dev-toolbar-end">
          <button
            type="button"
            className="icon-btn"
            onClick={() => void refreshDevices()}
            aria-label="刷新设备列表"
            title="刷新设备列表"
          >
            <RefreshCw size={16} className={refreshing ? "spin" : ""} />
          </button>
          <button type="button" className="btn btn-ghost" onClick={openDirect}>
            <Cable size={15} />
            直连
          </button>
          <button type="button" className="btn btn-primary" onClick={openAdd}>
            <Plus size={15} />
            添加设备
          </button>
        </div>
      </div>

      <section
        className="dev-table"
        aria-label="设备列表"
        aria-busy={refreshing}
      >
        <div className="dev-head" aria-hidden="true">
          <span>设备</span>
          <span>状态</span>
          <span className="dev-col-ip">IP</span>
          <span className="dev-col-seen">最后在线</span>
          <span className="dev-col-wol" title="设备是否在线">
            电源
          </span>
          <span className="dev-head-actions">操作</span>
        </div>

        {refreshing ? (
          <div className="dev-skeleton" aria-label="正在刷新设备列表">
            {[0, 1, 2, 3].map((row) => (
              <div className="dev-row dev-row-skeleton" key={row}>
                <span className="skeleton dev-skel-name" />
                <span className="skeleton dev-skel-sm" />
                <span className="skeleton dev-skel-sm dev-col-ip" />
                <span className="skeleton dev-skel-sm dev-col-seen" />
                <span className="skeleton dev-skel-sm dev-col-wol" />
                <span className="skeleton dev-skel-sm" />
              </div>
            ))}
          </div>
        ) : filteredDevices.length === 0 ? (
          <div className="dev-empty">
            {filterActive ? (
              <EmptyState
                icon={<Search size={20} />}
                title="没有符合条件的设备"
                hint="调整搜索关键词或筛选条件后再试。"
                action={
                  <button
                    type="button"
                    className="btn btn-ghost"
                    onClick={clearFilters}
                  >
                    清除筛选
                  </button>
                }
              />
            ) : (
              <EmptyState
                icon={<MonitorSmartphone size={20} />}
                title="还没有其他设备"
                hint="在另一台电脑上安装 ScreenWall 并登录同一账号，设备联网后会自动出现在这里。"
                action={
                  <button
                    type="button"
                    className="btn btn-primary"
                    onClick={openAdd}
                  >
                    <Plus size={15} />
                    添加设备
                  </button>
                }
              />
            )}
          </div>
        ) : (
          filteredDevices.map((device) => {
            const isLocal = device.id === account.deviceId;
            const controllable =
              controlState[device.id] ?? device.controllable;
            const expanded = expandedId === device.id;
            const statusText = !device.online
              ? "离线"
              : controllable
                ? "在线"
                : "仅查看";
            const statusTone = !device.online
              ? "offline"
              : controllable
                ? "online"
                : "view";

            return (
              <Fragment key={device.id}>
                <div
                  className={`dev-row ${device.online ? "" : "is-offline"} ${
                    expanded ? "is-expanded" : ""
                  }`}
                >
                  <div className="dev-cell-device">
                    <div className="dev-ident">
                      <span className="dev-name-row">
                        <strong>{displayName(device)}</strong>
                        {isLocal && (
                          <span className="badge badge-muted">本机</span>
                        )}
                      </span>
                      <span className="dev-os">
                        {device.os}
                        {device.version ? ` · v${device.version}` : ""}
                      </span>
                    </div>
                  </div>

                  <span className={`dev-status is-${statusTone}`}>
                    <i aria-hidden="true" />
                    {statusText}
                  </span>

                  <span className="dev-cell-text dev-col-ip">
                    {device.ip ?? "—"}
                  </span>

                  <span className="dev-cell-text dev-col-seen">
                    {device.online
                      ? "当前在线"
                      : formatLastSeen(device.lastSeen)}
                  </span>

                  <span className="dev-wol-cell">
                    {device.online ? (
                      <span className="dev-muted">已开机</span>
                    ) : (
                      <button
                        type="button"
                        className="dev-wake"
                        title="远程开机（Wake-on-LAN）"
                        aria-label={`远程开机 ${displayName(device)}`}
                        onClick={() => openWake(device)}
                      >
                        <Power size={13} strokeWidth={1.9} />
                        唤醒
                      </button>
                    )}
                  </span>

                  <div className="dev-actions">
                    <button
                      type="button"
                      className="dev-action"
                      title="远程控制"
                      aria-label={`远程控制 ${displayName(device)}`}
                      onClick={() => controlDevice(device)}
                    >
                      <MonitorPlay size={15} />
                    </button>
                    <button
                      type="button"
                      className="dev-action"
                      title="文件传输"
                      aria-label={`打开 ${displayName(device)} 的文件传输`}
                      onClick={() => openFiles(device)}
                    >
                      <FolderOpen size={15} />
                    </button>
                    <button
                      type="button"
                      className={`dev-action ${expanded ? "is-active" : ""}`}
                      title="设备详情"
                      aria-label={`${expanded ? "收起" : "展开"} ${
                        displayName(device)
                      } 详情`}
                      aria-expanded={expanded}
                      onClick={() =>
                        setExpandedId(expanded ? null : device.id)
                      }
                    >
                      <MoreHorizontal size={16} />
                    </button>
                  </div>
                </div>

                {expanded && (
                  <div className="dev-detail">
                    <div className="dev-detail-facts">
                      <div className="dev-fact">
                        <span>设备 ID</span>
                        <code>{device.id}</code>
                      </div>
                      <div className="dev-fact">
                        <span>系统版本</span>
                        <span>
                          {device.os} · {device.version}
                        </span>
                      </div>
                      <div className="dev-fact">
                        <span>分辨率</span>
                        <span>{device.resolution ?? "—"}</span>
                      </div>
                      <div className="dev-fact">
                        <span>延迟</span>
                        <span>
                          {device.latency ? `${device.latency} ms` : "—"}
                        </span>
                      </div>
                    </div>

                    <div className="dev-detail-actions">
                      <Switch
                        label="允许远程控制"
                        hint="关闭后该设备仅可查看画面"
                        checked={controllable}
                        disabled={controlBusy[device.id] === true}
                        onChange={(value) => void toggleControl(device, value)}
                      />
                      <div className="dev-detail-buttons">
                        <button
                          type="button"
                          className="btn btn-ghost btn-sm"
                          onClick={() => openRename(device)}
                        >
                          <PencilLine size={14} />
                          重命名
                        </button>
                        {device.id !== account.deviceId && (
                          <button
                            type="button"
                            className="btn btn-ghost btn-sm is-danger"
                            onClick={() => setDeleteTarget(device)}
                          >
                            <Trash2 size={14} />
                            移除
                          </button>
                        )}
                      </div>
                    </div>
                  </div>
                )}
              </Fragment>
            );
          })
        )}
      </section>

      <Modal
        open={addOpen}
        onClose={() => setAddOpen(false)}
        title="添加设备"
        subtitle="无需手动配对，登录同一账号即可自动上线。"
        width={500}
        footer={
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => setAddOpen(false)}
          >
            知道了
          </button>
        }
      >
        <div className="dev-guide">
          <div className="dev-guide-row">
            <span className="dev-guide-icon">
              <MonitorSmartphone size={16} />
            </span>
            <div className="dev-guide-copy">
              <strong>安装 ScreenWall</strong>
              <p>在另一台 Windows 10 / 11 电脑上安装 ScreenWall 客户端。</p>
            </div>
          </div>
          <div className="dev-guide-row">
            <span className="dev-guide-icon">
              <LogIn size={16} />
            </span>
            <div className="dev-guide-copy">
              <strong>登录同一账号</strong>
              <p>
                使用账号「{account.username || "当前账号"}」登录，设备会继承账号权限。
              </p>
            </div>
          </div>
          <div className="dev-guide-row">
            <span className="dev-guide-icon">
              <Wifi size={16} />
            </span>
            <div className="dev-guide-copy">
              <strong>自动上线</strong>
              <p>设备联网后自动出现在列表中，无需输入 IP 或配对码。</p>
            </div>
          </div>
        </div>

        <div className="dev-id-block">
          <div className="dev-id-label">
            <span>本机设备 ID</span>
            <span className="badge badge-muted">本机</span>
          </div>
          <div className="dev-id-row">
            <code>{account.deviceId || "未获取"}</code>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={copyDeviceId}
              disabled={!account.deviceId}
            >
              {copied ? (
                <>
                  <Check size={14} />
                  已复制
                </>
              ) : (
                <>
                  <Copy size={14} />
                  复制
                </>
              )}
            </button>
          </div>
          <p className="dev-id-hint">
            用于在另一台设备上核对账号归属，请勿公开分享。
          </p>
        </div>
      </Modal>

      <Modal
        open={directOpen}
        onClose={() => {
          if (!directBusy) setDirectOpen(false);
        }}
        title="局域网直连"
        subtitle="同一网段的电脑可以不经服务器直接连接，不需要登录账号。"
        width={460}
        footer={
          <>
            <button
              type="button"
              className="btn btn-ghost"
              disabled={directBusy}
              onClick={() => setDirectOpen(false)}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-primary"
              disabled={directBusy || !directHost.trim()}
              onClick={() => void submitDirect()}
            >
              {directBusy ? "连接中…" : "连接"}
            </button>
          </>
        }
      >
        <div className="dev-direct-form">
          <p className="dev-direct-hint">
            对方在「设置 → 连接 → 局域网直连」里能看到本机地址和配对码。
            首次连接需要配对码，配对过的设备下次可直接连接。
          </p>

          <div className="dev-direct-scan">
            <button
              type="button"
              className="btn btn-ghost"
              onClick={() => void scanLan()}
              disabled={scanBusy}
            >
              {scanBusy ? (
                <>
                  <RefreshCw size={14} className="spin" />
                  扫描中…
                </>
              ) : (
                <>
                  <Wifi size={14} />
                  扫描同网段设备
                </>
              )}
            </button>
            {scanList !== null && scanList.length === 0 && (
              <p className="dev-direct-note">
                没有发现同网段设备，请确认对方已开启局域网直连且与本机在同一网段。
              </p>
            )}
            {scanList !== null && scanList.length > 0 && (
              <ul className="dev-scan-list">
                {scanList.map((item) => (
                  <li key={item.address}>
                    <button
                      type="button"
                      className={`dev-scan-item ${
                        directHost === item.address ? "is-active" : ""
                      }`}
                      onClick={() => {
                        setDirectHost(item.address);
                        setDirectError(null);
                      }}
                    >
                      <strong>{item.name || item.address}</strong>
                      <span>{item.address}</span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>
          <label className="field">
            <span>对方地址</span>
            <div className="field-control">
              <Cable size={15} />
              <input
                value={directHost}
                onChange={(event) => {
                  setDirectHost(event.target.value);
                  if (directError) setDirectError(null);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") submitDirect();
                }}
                placeholder="192.168.1.20 或 192.168.1.20:47900"
                spellCheck={false}
                autoComplete="off"
                autoFocus
              />
            </div>
            <span className="dev-direct-note">不填端口时使用 {DIRECT_PORT}</span>
          </label>
          <label className="field">
            <span>配对码（可选）</span>
            <div className="field-control">
              <KeyRound size={15} />
              <input
                value={directCode}
                onChange={(event) => {
                  // 只保留数字，最多 6 位。
                  const next = event.target.value.replace(/\D/g, "").slice(0, 6);
                  setDirectCode(next);
                  if (directError) setDirectError(null);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") submitDirect();
                }}
                placeholder="6 位数字"
                inputMode="numeric"
                maxLength={6}
                autoComplete="off"
                aria-label="配对码"
              />
            </div>
            <span className="dev-direct-note">已配对过的设备留空即可</span>
          </label>

          {directError && (
            <p className="dev-direct-error" role="alert">
              {directError}
            </p>
          )}
        </div>
      </Modal>

      <Modal
        open={renameTarget !== null}
        onClose={() => setRenameTarget(null)}
        title="重命名设备"
        subtitle={renameTarget ? `设备 ID：${renameTarget.id}` : undefined}
        footer={
          <>
            <button
              type="button"
              className="btn btn-ghost"
              onClick={() => setRenameTarget(null)}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-primary"
              onClick={() => void submitRename()}
              disabled={!renameDraft.trim() || renameBusy}
            >
              {renameBusy ? "保存中…" : "保存名称"}
            </button>
          </>
        }
      >
        <label className="field">
          <span>设备名称</span>
          <div className="field-control">
            <PencilLine size={15} />
            <input
              value={renameDraft}
              onChange={(event) => setRenameDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") submitRename();
              }}
              placeholder="例如：书房 · 主力机"
              maxLength={24}
              autoFocus
            />
          </div>
        </label>
      </Modal>

      <Modal
        open={wakeTarget !== null}
        onClose={() => {
          if (!wakeBusy) setWakeTarget(null);
        }}
        title="远程开机"
        subtitle={wakeTarget ? `目标：${displayName(wakeTarget)}` : undefined}
        footer={
          <>
            <button
              type="button"
              className="btn btn-ghost"
              disabled={wakeBusy}
              onClick={() => setWakeTarget(null)}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-primary"
              disabled={wakeBusy}
              onClick={() => void submitWake()}
            >
              {wakeBusy ? "发送中…" : "发送唤醒包"}
            </button>
          </>
        }
      >
        <div className="dev-wake-form">
          <p className="dev-wake-hint">
            {wakeHelper
              ? "唤醒包由所选设备代发：它必须和目标在同一局域网，发出前会在那台电脑上弹出授权请求，需要有人点「允许」。"
              : "唤醒包由这台电脑发出，目标必须与本机在同一局域网（或路由器已配置定向广播）。"}
            目标还需在 BIOS 和网卡属性中开启「网络唤醒 / Wake on Magic Packet」。
          </p>
          {wakeHelpers.length > 0 && (
            <label className="field">
              <span>发送方式</span>
              <div className="field-control">
                <select
                  value={wakeHelper}
                  onChange={(event) => setWakeHelper(event.target.value)}
                  aria-label="选择唤醒包发送方式"
                >
                  <option value="">由这台电脑广播（同网段）</option>
                  {wakeHelpers.map((device) => (
                    <option key={device.id} value={device.id}>
                      通过「{device.name}」代发（异地）
                    </option>
                  ))}
                </select>
              </div>
            </label>
          )}
          <label className="field">
            <span>目标网卡 MAC</span>
            <div className="field-control">
              <input
                value={wakeMac}
                onChange={(event) => setWakeMac(event.target.value)}
                placeholder="AA:BB:CC:DD:EE:FF"
                autoFocus
              />
            </div>
          </label>
          <label className="field">
            <span>广播地址</span>
            <div className="field-control">
              <input
                value={wakeBroadcast}
                onChange={(event) => setWakeBroadcast(event.target.value)}
                placeholder="255.255.255.255"
              />
            </div>
          </label>
        </div>
      </Modal>

      <Modal
        open={deleteTarget !== null}
        onClose={() => setDeleteTarget(null)}
        title="删除设备"
        subtitle={deleteTarget ? `设备 ID：${deleteTarget.id}` : undefined}
        width={440}
        footer={
          <>
            <button
              type="button"
              className="btn btn-ghost"
              onClick={() => setDeleteTarget(null)}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-danger"
              onClick={() => void confirmDelete()}
              disabled={deleteBusy}
            >
              <Trash2 size={14} />
              {deleteBusy ? "删除中…" : "删除设备"}
            </button>
          </>
        }
      >
        <div className="dev-confirm">
          <span className="dev-confirm-icon">
            <ShieldAlert size={20} />
          </span>
          <div>
            <strong>
              确定要删除「{deleteTarget ? displayName(deleteTarget) : ""}」吗？
            </strong>
            <p>
              设备会从账号中移除、令牌立即吊销，正在进行的会话会断开；该设备下次登录同一账号会重新出现在列表里。
            </p>
          </div>
        </div>
      </Modal>
    </div>
  );
}
