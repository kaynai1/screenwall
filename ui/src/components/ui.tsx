import { useEffect, useRef, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";

export function Switch({
  checked,
  onChange,
  label,
  hint,
  disabled = false,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label?: string;
  hint?: string;
  disabled?: boolean;
}) {
  return (
    <label className={`switch-row ${disabled ? "is-disabled" : ""}`}>
      {(label || hint) && (
        <span className="switch-copy">
          {label && <span className="switch-label">{label}</span>}
          {hint && <span className="switch-hint">{hint}</span>}
        </span>
      )}
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        aria-label={label ?? "切换"}
        className={`switch ${checked ? "is-on" : ""}`}
        disabled={disabled}
        onClick={() => onChange(!checked)}
      >
        <span className="switch-thumb" />
      </button>
    </label>
  );
}

export function Segmented<T extends string>({
  value,
  options,
  onChange,
  size = "md",
}: {
  value: T;
  options: { value: T; label: string; icon?: ReactNode }[];
  onChange: (value: T) => void;
  size?: "sm" | "md";
}) {
  return (
    <div className={`segmented segmented-${size}`}>
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          className={`segmented-item ${option.value === value ? "is-active" : ""}`}
          onClick={() => onChange(option.value)}
        >
          {option.icon}
          <span>{option.label}</span>
        </button>
      ))}
    </div>
  );
}

export function Progress({
  value,
  tone = "mint",
  thin = false,
}: {
  value: number;
  tone?: "mint" | "blue" | "amber" | "red" | "neutral";
  thin?: boolean;
}) {
  const clamped = Math.min(100, Math.max(0, value));
  return (
    <span className={`progress progress-${tone} ${thin ? "progress-thin" : ""}`}>
      <span className="progress-fill" style={{ width: `${clamped}%` }} />
    </span>
  );
}

export function EmptyState({
  icon,
  title,
  hint,
  action,
}: {
  icon: ReactNode;
  title: string;
  hint?: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <div className="empty-icon">{icon}</div>
      <h3>{title}</h3>
      {hint && <p>{hint}</p>}
      {action && <div className="empty-action">{action}</div>}
    </div>
  );
}

/** 弹窗里可聚焦的元素（隐藏/禁用/负 tabIndex 的跳过）。 */
const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function focusableItems(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (item) =>
      item.offsetParent !== null &&
      !item.hasAttribute("disabled") &&
      item.getAttribute("aria-hidden") !== "true",
  );
}

export function Modal({
  open,
  onClose,
  title,
  subtitle,
  children,
  footer,
  width = 460,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  subtitle?: string;
  children: ReactNode;
  footer?: ReactNode;
  width?: number;
}) {
  const cardRef = useRef<HTMLDivElement | null>(null);
  // 打开前的焦点：关闭后要还回去（通常是触发按钮）。
  const restoreRef = useRef<HTMLElement | null>(null);
  // 最新的 onClose，避免把 effect 的依赖写成回调导致重复挂载。
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  useEffect(() => {
    if (!open) return;
    const previous = document.activeElement;
    restoreRef.current = previous instanceof HTMLElement ? previous : null;

    const card = cardRef.current;
    if (card) {
      // 焦点移进弹窗：优先第一个可操作元素，没有就落在弹窗本身。
      const items = focusableItems(card);
      const target = items[0] ?? card;
      target.focus({ preventScroll: true });
    }

    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        closeRef.current();
        return;
      }
      if (event.key !== "Tab" || !cardRef.current) return;
      // 焦点陷阱：Tab / Shift+Tab 在弹窗内循环。
      const items = focusableItems(cardRef.current);
      if (items.length === 0) {
        event.preventDefault();
        cardRef.current.focus({ preventScroll: true });
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement;
      if (!event.shiftKey && (active === last || !cardRef.current.contains(active))) {
        event.preventDefault();
        first.focus({ preventScroll: true });
      } else if (event.shiftKey && (active === first || !cardRef.current.contains(active))) {
        event.preventDefault();
        last.focus({ preventScroll: true });
      }
    };
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      // 关闭后把焦点还给触发元素；元素已经不在了就退回 body。
      const restore = restoreRef.current;
      restoreRef.current = null;
      if (restore && document.contains(restore)) {
        restore.focus({ preventScroll: true });
      }
    };
  }, [open]);

  if (!open) return null;

  return createPortal(
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div
        ref={cardRef}
        className="modal-card"
        style={{ width }}
        onMouseDown={(event) => event.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
      >
        <div className="modal-head">
          <div>
            <h2>{title}</h2>
            {subtitle && <p>{subtitle}</p>}
          </div>
          <button type="button" className="icon-btn" onClick={onClose} aria-label="关闭">
            <X size={17} />
          </button>
        </div>
        <div className="modal-body">{children}</div>
        {footer && <div className="modal-foot">{footer}</div>}
      </div>
    </div>,
    document.body,
  );
}
