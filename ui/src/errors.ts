/**
 * 错误文案统一入口。
 *
 * 后端的错误有三类来源：
 * 1. 协议错误码（`crates/proto/src/lib.rs` 的 `codes`，以及 `docs/server.md` 的 42xx）；
 * 2. Rust 命令直接返回的中文串（`invoke` 会包成 `Error: ...`）；
 * 3. HTTP API 的错误体（`{ error: "invalid_token", msg: "..." }`）。
 *
 * `describeError` 把这三类都归一成 `{ title, detail }`，界面上永远给用户看中文人话，
 * 不再出现 `Error: invoke failed: 4100`。
 */

export type ErrorCopy = {
  /** 一句话结论，用于标题或 Toast 开头。 */
  title: string;
  /** 具体说明 + 下一步怎么办。 */
  detail: string;
  /** 可选的行动提示（「重试」「重新登录」这类按钮文案）。 */
  action?: string;
};

/** 协议层错误码（`crates/proto/src/lib.rs` 的 `codes` 模块，全部列出）。 */
const PROTOCOL_CODES: Record<number, ErrorCopy> = {
  // 握手失败（WebSocket 关闭码 / 错误消息）
  4001: {
    title: "登录已失效",
    detail: "服务器不认识当前账号令牌，请退出后重新登录。",
    action: "重新登录",
  },
  4002: {
    title: "协议版本不匹配",
    detail: "客户端与被控端/服务器的协议版本不一致，请把两端都升级到最新版本。",
  },
  4003: {
    title: "连接数已满",
    detail: "当前账号的并发连接或订阅数超过了服务器上限，关掉不用的会话后再试。",
  },
  4004: {
    // 被控端用这个码同时表示「关了允许被控」和「人点了拒绝/超时没点」，
    // 旧文案只提开关，看到「被控端拒绝了本次请求」时会误以为是自己关了开关。
    title: "被控端拒绝了请求",
    detail:
      "对方没有允许这次控制，或者那边的「允许被控」开关是关着的。到对方电脑上确认后重新连接。",
  },
  4005: {
    title: "握手超时",
    detail: "连接建立后迟迟没有完成握手，通常是网络不稳定，稍后重试即可。",
    action: "重试",
  },
  4006: {
    title: "登录尝试过于频繁",
    detail: "密码错误次数过多，账号被临时限制，请等几分钟再登录。",
  },

  // 应用层错误（`t = error`）
  4100: {
    title: "请求参数有误",
    detail: "这次操作的参数不合法，请检查输入后重试。",
  },
  4101: {
    title: "超过订阅上限",
    detail: "同时订阅的画面或声音太多了，先关掉一路再打开新的。",
  },
  4102: {
    title: "地址不允许",
    detail: "被控端拒绝了这次的目标地址或 MAC，请检查地址格式与网段。",
  },
  4103: {
    title: "显示器不存在",
    detail: "要切换的那块显示器已经拔掉或编号变了，重新打开画面设置再选一次。",
  },
  4104: {
    title: "编码器不可用",
    detail: "被控端没能启动画面编码（显卡驱动或桌面会话异常），重启被控端后再试。",
  },
  4105: {
    title: "文件路径不允许",
    detail: "这个路径不在允许传输的目录里，换一个目录再试。",
  },
  4106: {
    title: "磁盘空间不足",
    detail: "被控端或本机磁盘写满了，清理空间后重新传输。",
  },
  4107: {
    title: "文件校验失败",
    detail: "传输过程中数据损坏，任务已中断，请重新发起传输。",
    action: "重新传输",
  },
  4108: {
    title: "控制已关闭",
    detail: "被控端关闭了远程控制功能，需要在那台电脑上重新允许。",
  },
  4109: {
    title: "对端不支持该功能",
    detail: "被控端版本较旧，不认识这条消息；升级被控端后即可使用。",
  },
  4110: {
    title: "系统声音不可用",
    detail: "被控端没有可用的声音设备，或当前格式不受支持。画面与键鼠不受影响。",
  },
  4111: {
    title: "电源操作失败",
    detail: "被控端无法执行关机/重启（可能没有权限或系统策略限制）。",
  },
};

/** 服务器中转层错误码（`docs/server.md` 第 11 节）。 */
const SERVER_CODES: Record<number, ErrorCopy> = {
  4200: {
    title: "设备不在线",
    detail: "目标设备当前离线或已被移除，等它上线后再发起连接。",
  },
  4201: {
    title: "会话已断开",
    detail: "会话被对端关闭或设备掉线，正在尝试自动恢复。",
    action: "重连",
  },
  4202: {
    title: "画面数据过大",
    detail: "单帧超过了服务器允许的上限，降低分辨率或画质后再试。",
  },
  4203: {
    title: "会话号无效",
    detail: "这条会话在服务器上已经不存在，重新发起连接即可。",
    action: "重连",
  },
  4204: {
    title: "设备拒绝了会话",
    detail: "对方没有点击「允许控制」，或已经关闭了被控开关。",
    action: "重新请求",
  },
  4205: {
    title: "连接过于频繁",
    detail: "短时间内建立的会话太多，被服务器限流了，稍等几秒再试。",
    action: "稍后重试",
  },
};

/** HTTP API 返回的 `error` 字段（服务器 `/api/v1/*`）。 */
const HTTP_ERRORS: Record<string, ErrorCopy> = {
  invalid_token: {
    title: "登录已失效",
    detail: "服务器上的登录状态已过期，请重新登录。",
    action: "重新登录",
  },
  auth_failed: {
    title: "用户名或密码错误",
    detail: "检查账号密码后再试；连续失败过多会被临时限制登录。",
  },
  rate_limited: {
    title: "登录尝试过于频繁",
    detail: "密码错误次数过多，账号被临时限制，请等几分钟再登录。",
  },
  not_found: {
    title: "设备不存在",
    detail: "目标设备已被移除，或在别的账号下，刷新设备列表后再试。",
  },
  conflict: {
    title: "操作冲突",
    detail: "服务器上的数据刚被其他端改过，刷新后再试。",
  },
  username_taken: {
    title: "这个用户名已经有人用了",
    detail: "如果这本来就是你注册的账号，切到「登录」用原来的密码登录；不是的话换一个用户名再注册。",
  },
  invalid_input: {
    title: "输入不合法",
    detail: "请检查填写的内容（长度、格式）后重试。",
  },
  internal: {
    title: "服务器内部错误",
    detail: "服务器处理这次请求时出错，稍后重试；持续出现请查看服务器日志。",
  },
};

/**
 * 常见的纯文本错误片段 → 统一文案。
 *
 * Rust 侧有些命令直接返回中文串（例如「主控连接已断开」），
 * 这些串本身已经是人话，只需要补一个标题。
 */
const MESSAGE_HINTS: { match: RegExp; copy: ErrorCopy }[] = [
  {
    // 注册时用户名已被占用：服务器回 409 + 「用户名已存在」。这条必须排在
    // HTTP 状态映射前面，否则会被统一说成「操作冲突 / 设备被其他端修改过」。
    match: /用户名已存在|用户名已被占用|用户名.*已注册/,
    copy: HTTP_ERRORS.username_taken,
  },
  {
    match: /尚未登录|未登录|请先登录/,
    copy: {
      title: "尚未登录",
      detail: "请先登录账号，再执行这个操作。",
      action: "去登录",
    },
  },
  {
    match: /登录已失效|令牌.*(失效|无效)|invalid token/i,
    copy: HTTP_ERRORS.invalid_token,
  },
  {
    match: /用户名或密码错误|密码错误/,
    copy: HTTP_ERRORS.auth_failed,
  },
  {
    match: /服务器尚未连接|主控连接尚未就绪|无法连接服务器|连接服务器失败/,
    copy: {
      title: "暂时连不上服务器",
      detail: "本机到服务器的连接还没建立，客户端会自动重试；也可以检查网络与服务器地址。",
      action: "重试",
    },
  },
  {
    match: /主控连接已断开|控制会话尚未建立|控制会话已切换/,
    copy: {
      title: "远程会话已断开",
      detail: "到被控端的会话已失效，正在自动重连；如果长时间不恢复可以手动重试。",
      action: "重连",
    },
  },
  {
    match: /设备不存在|设备离线|不在当前账号下/,
    copy: SERVER_CODES[4200],
  },
  {
    match: /被控端.*(拒绝|未确认|超时)|没有允许/,
    copy: SERVER_CODES[4204],
  },
  {
    match: /显示器.*不存在/,
    copy: PROTOCOL_CODES[4103],
  },
  {
    match: /磁盘|空间不足/,
    copy: PROTOCOL_CODES[4106],
  },
  {
    match: /校验失败|sha256|checksum/i,
    copy: PROTOCOL_CODES[4107],
  },
  {
    match: /系统声音|音频.*(不可用|失败)/,
    copy: PROTOCOL_CODES[4110],
  },
  {
    match: /电源操作失败|无法启动 shutdown/i,
    copy: PROTOCOL_CODES[4111],
  },
  {
    match: /网络|timeout|timed out|超时|ECONN|fetch failed/i,
    copy: {
      title: "网络异常",
      detail: "这次请求没有走完网络，检查网络连接后再试。",
      action: "重试",
    },
  },
];

const FALLBACK: ErrorCopy = {
  title: "操作失败",
  detail: "这次操作没有完成，稍后重试；如果一直失败，可以在设置里导出诊断包。",
  action: "重试",
};

/** 从任意错误对象里抠出可读文本。 */
function rawText(input: unknown): string {
  if (input === null || input === undefined) return "";
  if (typeof input === "string") return input;
  if (typeof input === "number") return String(input);
  if (input instanceof Error) return input.message || input.name;
  if (typeof input === "object") {
    const record = input as Record<string, unknown>;
    for (const key of ["message", "msg", "detail", "error", "reason"]) {
      const value = record[key];
      if (typeof value === "string" && value.trim()) return value;
    }
    try {
      return JSON.stringify(input);
    } catch {
      return String(input);
    }
  }
  return String(input);
}

/** 去掉 `Error:`、`invoke failed:` 这类包装，只留下正文。 */
function unwrap(text: string): string {
  let value = text.trim();
  for (const prefix of ["Error: ", "error: ", "Uncaught (in promise) "]) {
    while (value.startsWith(prefix)) value = value.slice(prefix.length).trim();
  }
  // Tauri 会把命令返回的 String 原样带出，偶尔带一层 `invoke failed: `。
  const invoke = value.match(/^invoke(?:\s+failed)?:\s*(.+)$/is);
  if (invoke) value = invoke[1].trim();
  return value;
}

/** 从「4123: 设备离线」或「4123 设备离线」里拆出码与文案。 */
function splitCode(text: string): { code: number | null; rest: string } {
  const match = text.match(/^(\d{4})\s*[:：-]?\s*(.*)$/s);
  if (!match) return { code: null, rest: text };
  const code = Number(match[1]);
  return Number.isFinite(code) ? { code, rest: match[2].trim() } : { code: null, rest: text };
}

function fromCode(code: number): ErrorCopy | null {
  return PROTOCOL_CODES[code] ?? SERVER_CODES[code] ?? null;
}

function fromHint(text: string): ErrorCopy | null {
  const found = MESSAGE_HINTS.find((hint) => hint.match.test(text));
  return found ? found.copy : null;
}

/**
 * 把任意错误输入归一成中文 `{ title, detail }`。
 *
 * 支持三种输入：
 * - 纯错误码：`4100` / `"4100"`；
 * - 「码: 文案」：`"4200: device offline"`（Rust 侧事件就是这么拼的）；
 * - 任意异常对象或字符串：`Error: invoke failed: ...`、HTTP 错误体等。
 */
export function describeError(codeOrMessage: unknown): ErrorCopy {
  const text = unwrap(rawText(codeOrMessage));

  // 1) 纯数字码。
  if (/^\d{4}$/.test(text)) {
    const hit = fromCode(Number(text));
    if (hit) return hit;
  }

  // 2) 「码: 文案」：优先按码给统一文案，码不认识再回落到文案本身。
  const { code, rest } = splitCode(text);
  if (code !== null) {
    const byCode = fromCode(code);
    if (byCode) {
      // 服务端文案有时比通用文案更具体（比如带剩余秒数），拼在说明里；
      // 纯英文/技术描述就不拼了，免得界面上又冒出 `device offline`。
      const useful = rest && /[一-龥]/.test(rest) && !rest.includes(byCode.title);
      return useful
        ? { ...byCode, detail: `${byCode.detail}（服务器：${rest}）` }
        : byCode;
    }
    if (rest) return describeError(rest);
  }

  // 3) HTTP 错误名（`invalid_token` 这种）。
  const httpKey = text.toLowerCase().replace(/[^a-z_]/g, "");
  if (HTTP_ERRORS[httpKey]) return HTTP_ERRORS[httpKey];

  // 4) 已知中文/英文片段。
  const byHint = fromHint(text);
  if (byHint) return byHint;

  // 5) HTTP 状态码：`…（HTTP 401）`。
  const httpStatus = text.match(/HTTP\s+(\d{3})/i);
  if (httpStatus) {
    const status = Number(httpStatus[1]);
    if (status === 401 || status === 403) return HTTP_ERRORS.invalid_token;
    if (status === 404) return HTTP_ERRORS.not_found;
    if (status === 409) return HTTP_ERRORS.conflict;
    if (status === 429) return HTTP_ERRORS.rate_limited;
    if (status >= 500) return HTTP_ERRORS.internal;
  }

  // 6) 剩下的正文已经是人话（Rust 命令返回的中文串）：给它配一个标题。
  if (text && /[一-龥]/.test(text) && text.length <= 120) {
    return { title: "操作失败", detail: text };
  }

  // 7) 实在认不出来：给出兜底文案，同时把原始信息留在说明里方便排查。
  if (text && text.length <= 200) {
    return { ...FALLBACK, detail: `${FALLBACK.detail}（原始信息：${text}）` };
  }
  return FALLBACK;
}

/** 便捷函数：直接拿到「标题：说明」一行，Toast 用。 */
export function errorLine(input: unknown): string {
  const { title, detail } = describeError(input);
  return `${title}：${detail}`;
}

/** 已知错误码清单（供单测/文档核对，覆盖 proto 与 server 两层）。 */
export const KNOWN_ERROR_CODES: number[] = [
  ...Object.keys(PROTOCOL_CODES),
  ...Object.keys(SERVER_CODES),
].map(Number);
