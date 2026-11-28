/**
 * 浏览器键鼠 / 触摸输入 → 协议 `input` 事件。
 *
 * 扫描码表与 `ui/src/input.ts` 保持一致（Windows Set 1）。
 */

export type ScanMapping = { scan: number; ext: boolean };

export type InputEvent =
  | { k: "move"; abs?: [number, number]; rel?: [number, number] }
  | { k: "button"; button: "left" | "right" | "middle"; down: boolean; abs?: [number, number] }
  | { k: "wheel"; delta: [number, number] }
  | { k: "key"; scan: number; down: boolean; ext?: boolean }
  | { k: "text"; text: string }
  | { k: "releaseAll" };

export const SCAN_CODES: Record<string, ScanMapping> = {
  Escape: { scan: 0x01, ext: false },
  Digit1: { scan: 0x02, ext: false },
  Digit2: { scan: 0x03, ext: false },
  Digit3: { scan: 0x04, ext: false },
  Digit4: { scan: 0x05, ext: false },
  Digit5: { scan: 0x06, ext: false },
  Digit6: { scan: 0x07, ext: false },
  Digit7: { scan: 0x08, ext: false },
  Digit8: { scan: 0x09, ext: false },
  Digit9: { scan: 0x0a, ext: false },
  Digit0: { scan: 0x0b, ext: false },
  Minus: { scan: 0x0c, ext: false },
  Equal: { scan: 0x0d, ext: false },
  Backspace: { scan: 0x0e, ext: false },
  Tab: { scan: 0x0f, ext: false },
  KeyQ: { scan: 0x10, ext: false },
  KeyW: { scan: 0x11, ext: false },
  KeyE: { scan: 0x12, ext: false },
  KeyR: { scan: 0x13, ext: false },
  KeyT: { scan: 0x14, ext: false },
  KeyY: { scan: 0x15, ext: false },
  KeyU: { scan: 0x16, ext: false },
  KeyI: { scan: 0x17, ext: false },
  KeyO: { scan: 0x18, ext: false },
  KeyP: { scan: 0x19, ext: false },
  BracketLeft: { scan: 0x1a, ext: false },
  BracketRight: { scan: 0x1b, ext: false },
  Enter: { scan: 0x1c, ext: false },
  ControlLeft: { scan: 0x1d, ext: false },
  KeyA: { scan: 0x1e, ext: false },
  KeyS: { scan: 0x1f, ext: false },
  KeyD: { scan: 0x20, ext: false },
  KeyF: { scan: 0x21, ext: false },
  KeyG: { scan: 0x22, ext: false },
  KeyH: { scan: 0x23, ext: false },
  KeyJ: { scan: 0x24, ext: false },
  KeyK: { scan: 0x25, ext: false },
  KeyL: { scan: 0x26, ext: false },
  Semicolon: { scan: 0x27, ext: false },
  Quote: { scan: 0x28, ext: false },
  Backquote: { scan: 0x29, ext: false },
  ShiftLeft: { scan: 0x2a, ext: false },
  Backslash: { scan: 0x2b, ext: false },
  KeyZ: { scan: 0x2c, ext: false },
  KeyX: { scan: 0x2d, ext: false },
  KeyC: { scan: 0x2e, ext: false },
  KeyV: { scan: 0x2f, ext: false },
  KeyB: { scan: 0x30, ext: false },
  KeyN: { scan: 0x31, ext: false },
  KeyM: { scan: 0x32, ext: false },
  Comma: { scan: 0x33, ext: false },
  Period: { scan: 0x34, ext: false },
  Slash: { scan: 0x35, ext: false },
  ShiftRight: { scan: 0x36, ext: false },
  NumpadMultiply: { scan: 0x37, ext: false },
  AltLeft: { scan: 0x38, ext: false },
  Space: { scan: 0x39, ext: false },
  CapsLock: { scan: 0x3a, ext: false },
  F1: { scan: 0x3b, ext: false },
  F2: { scan: 0x3c, ext: false },
  F3: { scan: 0x3d, ext: false },
  F4: { scan: 0x3e, ext: false },
  F5: { scan: 0x3f, ext: false },
  F6: { scan: 0x40, ext: false },
  F7: { scan: 0x41, ext: false },
  F8: { scan: 0x42, ext: false },
  F9: { scan: 0x43, ext: false },
  F10: { scan: 0x44, ext: false },
  ScrollLock: { scan: 0x46, ext: false },
  Numpad7: { scan: 0x47, ext: false },
  Numpad8: { scan: 0x48, ext: false },
  Numpad9: { scan: 0x49, ext: false },
  NumpadSubtract: { scan: 0x4a, ext: false },
  Numpad4: { scan: 0x4b, ext: false },
  Numpad5: { scan: 0x4c, ext: false },
  Numpad6: { scan: 0x4d, ext: false },
  NumpadAdd: { scan: 0x4e, ext: false },
  Numpad1: { scan: 0x4f, ext: false },
  Numpad2: { scan: 0x50, ext: false },
  Numpad3: { scan: 0x51, ext: false },
  Numpad0: { scan: 0x52, ext: false },
  NumpadDecimal: { scan: 0x53, ext: false },
  IntlBackslash: { scan: 0x56, ext: false },
  F11: { scan: 0x57, ext: false },
  F12: { scan: 0x58, ext: false },
  NumpadEqual: { scan: 0x59, ext: false },
  IntlRo: { scan: 0x73, ext: false },
  IntlYen: { scan: 0x7d, ext: false },
  NumpadEnter: { scan: 0x1c, ext: true },
  ControlRight: { scan: 0x1d, ext: true },
  NumpadDivide: { scan: 0x35, ext: true },
  PrintScreen: { scan: 0x37, ext: true },
  AltRight: { scan: 0x38, ext: true },
  NumLock: { scan: 0x45, ext: true },
  Home: { scan: 0x47, ext: true },
  ArrowUp: { scan: 0x48, ext: true },
  PageUp: { scan: 0x49, ext: true },
  ArrowLeft: { scan: 0x4b, ext: true },
  ArrowRight: { scan: 0x4d, ext: true },
  End: { scan: 0x4f, ext: true },
  ArrowDown: { scan: 0x50, ext: true },
  PageDown: { scan: 0x51, ext: true },
  Insert: { scan: 0x52, ext: true },
  Delete: { scan: 0x53, ext: true },
  MetaLeft: { scan: 0x5b, ext: true },
  MetaRight: { scan: 0x5c, ext: true },
  ContextMenu: { scan: 0x5d, ext: true },
};

/** 视口坐标 → 远端 0..1 归一化坐标（按 contain 扣掉留白）。 */
export function pointToNormalized(
  rect: { width: number; height: number; left: number; top: number },
  clientX: number,
  clientY: number,
  frameW: number,
  frameH: number,
): [number, number] | null {
  if (rect.width <= 0 || rect.height <= 0 || frameW <= 0 || frameH <= 0) return null;
  const scale = Math.min(rect.width / frameW, rect.height / frameH);
  const displayW = frameW * scale;
  const displayH = frameH * scale;
  const offsetX = rect.left + (rect.width - displayW) / 2;
  const offsetY = rect.top + (rect.height - displayH) / 2;
  const x = (clientX - offsetX) / displayW;
  const y = (clientY - offsetY) / displayH;
  if (x < -0.02 || y < -0.02 || x > 1.02 || y > 1.02) return null;
  const round = (value: number) => Math.round(Math.min(1, Math.max(0, value)) * 10000) / 10000;
  return [round(x), round(y)];
}

/** 滚轮像素增量 → Windows WHEEL_DELTA 刻度（向下为负）。 */
export function wheelNotches(value: number): number {
  if (!value) return 0;
  const steps = Math.trunc(value / 100);
  return (steps === 0 ? Math.sign(value) : steps) * 120;
}

/** 按 16ms 批量发送，连续 move 只保留最后一个，避免把 WS 打满。 */
export class InputBatcher {
  private queue: InputEvent[] = [];
  private timer: number | null = null;
  private readonly send: (events: InputEvent[]) => void;

  constructor(send: (events: InputEvent[]) => void) {
    this.send = send;
  }

  push(event: InputEvent): void {
    if (event.k === "move" && this.queue.length > 0) {
      const last = this.queue[this.queue.length - 1];
      if (last.k === "move") {
        this.queue[this.queue.length - 1] = event;
        this.schedule();
        return;
      }
    }
    this.queue.push(event);
    this.schedule();
  }

  /** 立即发掉队列（点击、按键这类不该等的输入）。 */
  flush(): void {
    if (this.timer !== null) {
      window.clearTimeout(this.timer);
      this.timer = null;
    }
    if (this.queue.length === 0) return;
    const events = this.queue;
    this.queue = [];
    this.send(events);
  }

  private schedule(): void {
    if (this.timer !== null) return;
    this.timer = window.setTimeout(() => {
      this.timer = null;
      this.flush();
    }, 16);
  }
}
