import { useEffect, useMemo, useRef, useState } from "react";
import {
  Check,
  CheckCircle2,
  Copy,
  Lightbulb,
  Loader2,
  Mail,
  Send,
  ShieldCheck,
  UsersRound,
  WifiOff,
} from "lucide-react";
import { submitFeedback, type FeedbackCategory } from "../api";
import { BugIcon } from "../components/QingIcons";
import { Segmented } from "../components/ui";
import { errorLine } from "../errors";
import type { FeedbackPageProps } from "../types";
import "./FeedbackPage.css";

const MAX_LENGTH = 4000;
const MIN_LENGTH = 5;

/** 官方联系方式：提交到服务器之外，想直接找人时用。 */
const QQ_GROUP = "128906393";
const OFFICIAL_EMAIL = "xqtban@88.com";

/** 按字符（而不是 UTF-16 码元）计数，和服务端的 `chars().count()` 一致。 */
function charCount(value: string): number {
  return Array.from(value.trim()).length;
}

export default function FeedbackPage({ account, active, notify }: FeedbackPageProps) {
  const [category, setCategory] = useState<FeedbackCategory>("suggestion");
  const [content, setContent] = useState("");
  const [contact, setContact] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sent, setSent] = useState(false);
  /** 刚复制的是哪一项：`qq` / `email`，2 秒后恢复。 */
  const [copied, setCopied] = useState<"qq" | "email" | null>(null);
  const copyTimer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (copyTimer.current !== null) window.clearTimeout(copyTimer.current);
    },
    [],
  );

  async function copyContact(what: "qq" | "email", value: string) {
    // 内嵌 WebView 不一定给 clipboard 权限，而且被拒绝时 Promise 可能一直不落定；
    // 用 800ms 超时兜底，再退回选中 + execCommand。
    const ok = await Promise.race([
      navigator.clipboard
        .writeText(value)
        .then(() => true)
        .catch(() => false),
      new Promise<boolean>((resolve) => window.setTimeout(() => resolve(false), 800)),
    ]).then((written) => {
      if (written) return true;
      const area = document.createElement("textarea");
      area.value = value;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      let copied = false;
      try {
        copied = document.execCommand("copy");
      } catch {
        copied = false;
      }
      document.body.removeChild(area);
      return copied;
    });
    if (!ok) {
      notify("复制失败，请手动选择", "error");
      return;
    }
    setCopied(what);
    if (copyTimer.current !== null) window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopied(null), 2000);
    notify(what === "qq" ? "群号已复制" : "邮箱已复制", "success");
  }

  const length = useMemo(() => charCount(content), [content]);
  const canSubmit =
    !busy && active && length >= MIN_LENGTH && length <= MAX_LENGTH;

  function changeContent(value: string) {
    setContent(value);
    // 改内容就收起「已提交」，避免用户改完以为已经发出去了。
    if (sent) setSent(false);
    if (error) setError(null);
  }

  async function handleSubmit() {
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      await submitFeedback({ category, content, contact });
      setSent(true);
      setContent("");
      setContact("");
      notify("反馈已提交，谢谢你的建议", "success");
    } catch (submitError) {
      setError(errorLine(submitError));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="fb-page">
      <section className="fb-intro">
        <div className="fb-intro-icon" aria-hidden="true">
          <BugIcon size={20} strokeWidth={1.75} />
        </div>
        <div className="fb-intro-copy">
          <h2>建议与 BUG</h2>
          <p>
            用着不顺手、想要什么功能、哪一步出了问题，都可以写在这里。
            反馈会直接送到服务器管理员手上，不需要你另外联系。
          </p>
        </div>
      </section>

      {account.local ? (
        // 局域网模式没有服务器可用，提交不出去；与其藏掉入口，不如说清楚怎么用。
        <section className="fb-card fb-offline">
          <div className="fb-offline-icon" aria-hidden="true">
            <WifiOff size={18} strokeWidth={1.75} />
          </div>
          <div>
            <h3>局域网模式连不上服务器</h3>
            <p>
              反馈要保存在服务器上，请在「设置 → 账号」里连接服务器并登录，
              再回来提交。
            </p>
          </div>
        </section>
      ) : (
      <section className="fb-card" aria-label="提交反馈">
        <div className="fb-row">
          <span className="fb-label">类型</span>
          <Segmented
            value={category}
            onChange={setCategory}
            size="sm"
            options={[
              {
                value: "suggestion",
                label: "建议",
                icon: <Lightbulb size={14} strokeWidth={1.75} />,
              },
              {
                value: "bug",
                label: "问题",
                icon: <BugIcon size={14} />,
              },
            ]}
          />
        </div>

        <label className="fb-field">
          <span className="fb-label">
            {category === "bug" ? "问题描述" : "你的建议"}
          </span>
          <textarea
            className="fb-textarea"
            value={content}
            maxLength={MAX_LENGTH}
            rows={8}
            placeholder={
              category === "bug"
                ? "例如：连接后画面卡住 / 文件传到一半中断。写清楚做了什么、看到什么，最好带上大概时间。"
                : "例如：希望控制页能记住上次选的画质，或者加一个快捷键。"
            }
            onChange={(event) => changeContent(event.target.value)}
            disabled={busy}
          />
          <span className="fb-count">
            {length} / {MAX_LENGTH}
          </span>
        </label>

        <label className="fb-field">
          <span className="fb-label">
            <Mail size={13} strokeWidth={1.75} />
            联系方式（可选）
          </span>
          <input
            className="fb-input"
            value={contact}
            maxLength={120}
            placeholder="邮箱 / QQ / 微信，方便进一步了解情况"
            onChange={(event) => setContact(event.target.value)}
            disabled={busy}
          />
        </label>

        {sent && (
          <p className="fb-note is-ok" role="status">
            <CheckCircle2 size={14} strokeWidth={1.75} />
            已收到你的反馈，管理员会在服务器上看到。
          </p>
        )}
        {error && (
          <p className="fb-note is-error" role="alert">
            {error}
          </p>
        )}

        <div className="fb-foot">
          <p className="fb-privacy">
            <ShieldCheck size={14} strokeWidth={1.75} />
            反馈只保存在你自建的服务器上，仅该服务器管理员可见，不会发给开发者；
            想给开发者提建议，请用下方的官方 QQ 群或邮箱。
          </p>
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => void handleSubmit()}
            disabled={!canSubmit}
          >
            {busy ? (
              <>
                <Loader2 size={14} className="spin" />
                提交中…
              </>
            ) : (
              <>
                <Send size={14} strokeWidth={1.75} />
                提交反馈
              </>
            )}
          </button>
        </div>
      </section>
      )}

      <section className="fb-card fb-contact" aria-label="官方联系方式">
        <h3 className="fb-contact-title">官方联系方式</h3>
        <div className="fb-contact-list">
          <div className="fb-contact-row">
            <span className="fb-contact-icon" aria-hidden="true">
              <UsersRound size={16} strokeWidth={1.75} />
            </span>
            <span className="fb-contact-copy">
              <small>官方 QQ 群</small>
              <strong>{QQ_GROUP}</strong>
            </span>
            <button
              type="button"
              className="btn btn-ghost fb-contact-btn"
              onClick={() => void copyContact("qq", QQ_GROUP)}
            >
              {copied === "qq" ? (
                <Check size={14} strokeWidth={1.75} />
              ) : (
                <Copy size={14} strokeWidth={1.75} />
              )}
              {copied === "qq" ? "已复制" : "复制群号"}
            </button>
          </div>
          <div className="fb-contact-row">
            <span className="fb-contact-icon" aria-hidden="true">
              <Mail size={16} strokeWidth={1.75} />
            </span>
            <span className="fb-contact-copy">
              <small>官方邮箱</small>
              <strong>{OFFICIAL_EMAIL}</strong>
            </span>
            <button
              type="button"
              className="btn btn-ghost fb-contact-btn"
              onClick={() => void copyContact("email", OFFICIAL_EMAIL)}
            >
              {copied === "email" ? (
                <Check size={14} strokeWidth={1.75} />
              ) : (
                <Copy size={14} strokeWidth={1.75} />
              )}
              {copied === "email" ? "已复制" : "复制邮箱"}
            </button>
          </div>
        </div>
        <p className="fb-contact-hint">
          QQ 群里可以和其他用户、开发者直接交流；邮箱适合不方便加群的情况。
          通过本页提交的内容只有当前服务器的管理员能看到。
        </p>
      </section>
    </div>
  );
}
