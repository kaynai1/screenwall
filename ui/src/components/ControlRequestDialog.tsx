import { useEffect, useRef, useState } from "react";
import { MonitorSmartphone, ShieldCheck } from "lucide-react";
import { Modal } from "./ui";
import type { ControlRequest } from "../api";

/** 「请求控制」是安全动作：点遮罩、按 Esc 都按拒绝处理，不能悄悄关掉。 */
export default function ControlRequestDialog({
  request,
  onAnswer,
}: {
  request: ControlRequest | null;
  onAnswer: (session: number, allow: boolean, reason?: "timeout") => void;
}) {
  const [left, setLeft] = useState(0);
  const answered = useRef<number | null>(null);

  const session = request?.session ?? null;
  const timeoutSec = request?.timeoutSec ?? 30;

  useEffect(() => {
    if (session === null) return;
    answered.current = null;
    const deadline = Date.now() + timeoutSec * 1000;
    setLeft(timeoutSec);
    const timer = window.setInterval(() => {
      const remain = Math.max(0, Math.ceil((deadline - Date.now()) / 1000));
      setLeft(remain);
      if (remain <= 0) {
        window.clearInterval(timer);
        if (answered.current !== session) {
          answered.current = session;
          onAnswer(session, false, "timeout");
        }
      }
    }, 250);
    return () => window.clearInterval(timer);
    // 同一会话只计一次时；换一条请求重新开始。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session, timeoutSec]);

  function answer(allow: boolean) {
    if (session === null) return;
    if (answered.current === session) return;
    answered.current = session;
    onAnswer(session, allow);
  }

  const ratio = timeoutSec > 0 ? Math.max(0, Math.min(1, left / timeoutSec)) : 0;

  return (
    <Modal
      open={request !== null}
      onClose={() => answer(false)}
      title="远程控制请求"
      subtitle={
        request
          ? `${request.viewer} 请求控制这台设备`
          : undefined
      }
      width={440}
      footer={
        <>
          <button
            type="button"
            className="btn btn-ghost"
            onClick={() => answer(false)}
          >
            拒绝
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => answer(true)}
          >
            允许控制
          </button>
        </>
      }
    >
      <div className="ctrl-request">
        <div className="ctrl-request-head">
          <span className="ctrl-request-icon" aria-hidden="true">
            <MonitorSmartphone size={20} strokeWidth={1.75} />
          </span>
          <div>
            <strong>允许后对方可看到本机画面并操作鼠标键盘</strong>
            <p>仅在你认识请求方时允许；会话结束或你断开后权限立即收回。</p>
          </div>
        </div>
        <div className="ctrl-request-countdown" role="status" aria-live="polite">
          <ShieldCheck size={15} strokeWidth={1.75} />
          <span>
            {left > 0 ? `${left} 秒后自动拒绝` : "已超时，正在拒绝…"}
          </span>
          <span className="ctrl-request-bar">
            <span
              className="ctrl-request-bar-fill"
              style={{ width: `${ratio * 100}%` }}
            />
          </span>
        </div>
      </div>
    </Modal>
  );
}
