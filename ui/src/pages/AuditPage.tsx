import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ChevronLeft,
  ChevronRight,
  History,
  RefreshCw,
  Search,
  ShieldCheck,
} from "lucide-react";
import { EmptyState } from "../components/ui";
import { describeError } from "../errors";
import {
  formatDuration,
  formatStarted,
  listAudits,
  type AuditEntry,
  type AuditPage as AuditPageData,
} from "../audit";
import { formatBytes } from "../mock";
import type { AuditPageProps, DeviceView } from "../types";
import "./AuditPage.css";

const PAGE_SIZE = 20;

type Filter = "all" | "active" | "finished";

const FILTERS: { value: Filter; label: string }[] = [
  { value: "all", label: "全部" },
  { value: "active", label: "进行中" },
  { value: "finished", label: "已结束" },
];

function deviceName(devices: DeviceView[], id: string): string {
  const found = devices.find((device) => device.id === id);
  if (found) return found.name;
  // 直连设备的 ID 是合成的 `direct:<ip>:<port>`，不在账号设备列表里，
  // 直接显示会比一串原始 ID 好读。
  if (id.startsWith("direct:")) {
    const rest = id.slice("direct:".length);
    const cut = rest.lastIndexOf(":");
    return cut > 0 ? `直连 ${rest.slice(0, cut)}` : "直连设备";
  }
  return id ? id : "未知设备";
}

export default function AuditPage({
  account,
  devices,
  notify,
}: AuditPageProps) {
  const [page, setPage] = useState<AuditPageData>({ audits: [], total: 0 });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [offset, setOffset] = useState(0);
  const [deviceId, setDeviceId] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const [query, setQuery] = useState("");
  const [refreshKey, setRefreshKey] = useState(0);
  // 进行中的会话要每秒重算时长，否则会一直停在打开页面时的那一秒。
  const [now, setNow] = useState(() => Date.now());

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const next = await listAudits({
        limit: PAGE_SIZE,
        offset,
        deviceId: deviceId || null,
      });
      setPage(next);
    } catch (err) {
      const { title, detail } = describeError(err);
      setError(`${title}：${detail}`);
      setPage({ audits: [], total: 0 });
    } finally {
      setLoading(false);
    }
  }, [offset, deviceId]);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  // 换过滤条件时回到第一页，否则可能停在空页。
  useEffect(() => {
    setOffset(0);
  }, [deviceId]);

  const hasLive = page.audits.some((row) => row.endedAt === null);
  useEffect(() => {
    if (!hasLive) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [hasLive]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return page.audits.filter((row) => {
      if (filter === "active" && row.endedAt !== null) return false;
      if (filter === "finished" && row.endedAt === null) return false;
      if (!needle) return true;
      return [
        deviceName(devices, row.controllerDeviceId),
        deviceName(devices, row.controlledDeviceId),
        row.controllerDeviceId,
        row.controlledDeviceId,
        String(row.sessionId),
        row.mode === "direct" ? "直连" : "中转",
      ]
        .join(" ")
        .toLowerCase()
        .includes(needle);
    });
  }, [page.audits, devices, filter, query]);

  const summary = useMemo(() => {
    const rows = page.audits;
    const active = rows.filter((row) => row.endedAt === null).length;
    const duration = rows.reduce((sum, row) => sum + row.durationMs, 0);
    const bytes = rows.reduce((sum, row) => sum + row.bytesUp + row.bytesDown, 0);
    return { active, duration, bytes };
  }, [page.audits]);

  const pageCount = Math.max(1, Math.ceil(page.total / PAGE_SIZE));
  const pageIndex = Math.floor(offset / PAGE_SIZE) + 1;
  const hasPrev = offset > 0;
  const hasNext = offset + PAGE_SIZE < page.total;

  function go(delta: number) {
    const next = Math.max(0, offset + delta * PAGE_SIZE);
    setOffset(next);
  }

  function refresh() {
    setRefreshKey((key) => key + 1);
    notify("正在刷新审计记录", "info");
  }

  const filterActive = filter !== "all" || query.trim().length > 0;

  function renderRow(row: AuditEntry) {
    const live = row.endedAt === null;
    return (
      <div
        className={`audit-row ${live ? "is-live" : ""}`}
        key={row.id}
        role="row"
      >
        <span className="audit-cell-time" role="cell">
          <strong>{formatStarted(row.startedAt)}</strong>
          <small>{row.mode === "direct" ? "直连" : `#${row.sessionId}`}</small>
        </span>

        <span className="audit-cell-pair" role="cell">
          <span className="audit-device">{deviceName(devices, row.controllerDeviceId)}</span>
          <span className="audit-arrow" aria-hidden="true">
            →
          </span>
          <span className="audit-device">{deviceName(devices, row.controlledDeviceId)}</span>
        </span>

        <span className="audit-cell-text" role="cell">
          {live
            ? formatDuration(now - row.startedAt)
            : formatDuration(row.durationMs)}
        </span>

        <span className="audit-cell-text" role="cell">{formatBytes(row.bytesUp)}</span>
        <span className="audit-cell-text" role="cell">{formatBytes(row.bytesDown)}</span>

        <span className={`audit-state ${live ? "is-live" : "is-done"}`} role="cell">
          <i aria-hidden="true" />
          {live ? "进行中" : "已结束"}
        </span>
      </div>
    );
  }

  return (
    <div className="audit-page">
      <div className="audit-summary">
        <div className="audit-stat">
          <span className="audit-stat-label">当前页会话</span>
          <strong>{page.audits.length}</strong>
        </div>
        <div className="audit-stat">
          <span className="audit-stat-label">进行中</span>
          <strong className={summary.active > 0 ? "is-live" : ""}>
            {summary.active}
          </strong>
        </div>
        <div className="audit-stat">
          <span className="audit-stat-label">累计时长</span>
          <strong>{formatDuration(summary.duration)}</strong>
        </div>
        <div className="audit-stat">
          <span className="audit-stat-label">累计流量</span>
          <strong>{formatBytes(summary.bytes)}</strong>
        </div>
        <div className="audit-stat">
          <span className="audit-stat-label">匹配记录</span>
          <strong>{page.total}</strong>
        </div>
      </div>

      <div className="audit-toolbar">
        <label className="field-control audit-search">
          <Search size={15} />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索设备或会话号"
            aria-label="搜索审计记录"
          />
        </label>

        <label className="field-control audit-device-filter">
          <History size={15} />
          <select
            value={deviceId}
            onChange={(event) => setDeviceId(event.target.value)}
            aria-label="按设备过滤"
          >
            <option value="">全部设备</option>
            {devices.map((device) => (
              <option key={device.id} value={device.id}>
                {device.name}
              </option>
            ))}
          </select>
        </label>

        <div className="segmented segmented-md audit-seg">
          {FILTERS.map((option) => (
            <button
              key={option.value}
              type="button"
              className={`segmented-item ${option.value === filter ? "is-active" : ""}`}
              onClick={() => setFilter(option.value)}
            >
              <span>{option.label}</span>
            </button>
          ))}
        </div>

        <div className="audit-toolbar-end">
          <button
            type="button"
            className="icon-btn"
            onClick={refresh}
            aria-label="刷新审计记录"
            title="刷新审计记录"
          >
            <RefreshCw size={16} className={loading ? "spin" : ""} />
          </button>
        </div>
      </div>

      <section
        className="audit-table"
        aria-label="会话审计记录"
        aria-busy={loading}
        role="table"
      >
        <div className="audit-head" role="row" aria-hidden="true">
          <span>开始时间</span>
          <span>主控 → 被控</span>
          <span>时长</span>
          <span>上行</span>
          <span>下行</span>
          <span>状态</span>
        </div>

        {error ? (
          <div className="audit-empty">
            <EmptyState
              icon={<ShieldCheck size={20} />}
              title="无法读取审计记录"
              hint={error}
              action={
                <button type="button" className="btn btn-ghost" onClick={refresh}>
                  重试
                </button>
              }
            />
          </div>
        ) : loading && page.audits.length === 0 ? (
          <div className="audit-skeleton" aria-label="正在加载审计记录">
            {[0, 1, 2, 3, 4].map((row) => (
              <div className="audit-row audit-row-skeleton" key={row}>
                <span className="skeleton audit-skel-sm" />
                <span className="skeleton audit-skel-wide" />
                <span className="skeleton audit-skel-sm" />
                <span className="skeleton audit-skel-sm" />
                <span className="skeleton audit-skel-sm" />
                <span className="skeleton audit-skel-sm" />
              </div>
            ))}
          </div>
        ) : visible.length === 0 ? (
          <div className="audit-empty">
            {filterActive || deviceId ? (
              <EmptyState
                icon={<Search size={20} />}
                title="没有符合条件的记录"
                hint="换一个设备、状态或关键词再试。"
                action={
                  <button
                    type="button"
                    className="btn btn-ghost"
                    onClick={() => {
                      setQuery("");
                      setFilter("all");
                      setDeviceId("");
                    }}
                  >
                    清除筛选
                  </button>
                }
              />
            ) : (
              <EmptyState
                icon={<History size={20} />}
                title="还没有会话记录"
                hint="任何一次远程控制、文件传输或群控会话结束后都会记录在这里。"
              />
            )}
          </div>
        ) : (
          visible.map(renderRow)
        )}
      </section>

      <div className="audit-foot">
        <span className="audit-foot-meta">
          第 {pageIndex} / {pageCount} 页 · 共 {page.total} 条
        </span>
        <div className="audit-pager">
          <button
            type="button"
            className="btn btn-ghost btn-sm"
            onClick={() => go(-1)}
            disabled={!hasPrev || loading}
          >
            <ChevronLeft size={14} />
            上一页
          </button>
          <button
            type="button"
            className="btn btn-ghost btn-sm"
            onClick={() => go(1)}
            disabled={!hasNext || loading}
          >
            下一页
            <ChevronRight size={14} />
          </button>
        </div>
      </div>

      <p className="audit-note">
        审计记录由服务器保存，只包含会话元数据（设备、起止时间、时长与流量），
        不包含画面、键鼠或文件内容。当前账号 {account.username || "—"}。
      </p>
    </div>
  );
}
