import { Link2 } from "lucide-react";
import { Modal } from "./ui";
import type { ConnectLinkInput } from "../api";

/**
 * 收到连接链接但当前已登录时先确认。
 *
 * 链接会顶掉账号会话（受限令牌只跑主控、不写盘），不能悄悄把用户登出。
 */
export default function ConnectLinkDialog({
  link,
  busy,
  onConfirm,
  onCancel,
}: {
  link: ConnectLinkInput | null;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  let host = link?.server ?? "";
  try {
    if (link) host = new URL(link.server).host;
  } catch {
    // 地址不合法时原样显示，不影响确认。
  }

  return (
    <Modal
      open={link !== null}
      onClose={onCancel}
      title="打开连接链接"
      subtitle={host || undefined}
      width={440}
      footer={
        <>
          <button type="button" className="btn btn-ghost" onClick={onCancel}>
            取消
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={onConfirm}
            disabled={busy}
          >
            断开并连接
          </button>
        </>
      }
    >
      <div className="ctrl-request">
        <div className="ctrl-request-head">
          <span className="ctrl-request-icon" aria-hidden="true">
            <Link2 size={20} strokeWidth={1.75} />
          </span>
          <div>
            <strong>这条链接会连接一台远程设备</strong>
            <p>
              打开后会退出当前账号，改用一次性链接建立远程会话。链接只能用一次，
              断开后需要对方重新生成。
            </p>
          </div>
        </div>
      </div>
    </Modal>
  );
}
