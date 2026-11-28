/** 协议 / 中转层错误码 → 中文文案（与 `ui/src/errors.ts` 保持同一套说法）。 */

const CODES: Record<number, { title: string; detail: string }> = {
  4001: { title: "登录已失效", detail: "服务器不认识当前令牌，请退出后重新登录。" },
  4002: { title: "协议版本不匹配", detail: "客户端与被控端版本差距过大，请升级两端。" },
  4003: { title: "连接数已满", detail: "并发会话超过服务器上限，关掉不用的会话再试。" },
  4004: { title: "对方关闭了被控", detail: "被控端关闭了「允许被控」开关。" },
  4005: { title: "握手超时", detail: "网络不稳定，稍后重试。" },
  4006: { title: "登录尝试过于频繁", detail: "请等几分钟再登录。" },
  4100: { title: "请求参数有误", detail: "这次操作的参数不合法。" },
  4101: { title: "超过订阅上限", detail: "同时订阅的画面太多了。" },
  4103: { title: "显示器不存在", detail: "目标显示器已拔掉或编号变了。" },
  4104: { title: "编码器不可用", detail: "被控端没能启动画面编码，重启被控端后再试。" },
  4108: { title: "控制已关闭", detail: "被控端关闭了远程控制功能。" },
  4109: { title: "对端不支持该功能", detail: "被控端版本较旧，升级后可用。" },
  4200: { title: "设备不在线", detail: "目标设备当前离线或已被移除。" },
  4201: { title: "会话已断开", detail: "会话被对端关闭或设备掉线。" },
  4202: { title: "画面数据过大", detail: "单帧超过服务器上限，降低分辨率或画质后再试。" },
  4203: { title: "会话号无效", detail: "这条会话已不存在，重新连接即可。" },
  4204: { title: "设备拒绝了会话", detail: "对方没有允许这次控制请求。" },
  4205: { title: "连接过于频繁", detail: "稍等几秒再试。" },
};

const HTTP_ERRORS: Record<string, { title: string; detail: string }> = {
  auth_failed: { title: "用户名或密码错误", detail: "检查账号密码后再试。" },
  invalid_token: { title: "登录已失效", detail: "请重新登录。" },
  rate_limited: { title: "尝试过于频繁", detail: "请等几分钟再试。" },
  registration_closed: { title: "服务器未开放注册", detail: "请联系服务器管理员开账号。" },
  device_offline: { title: "设备不在线", detail: "等那台电脑上线后再试。" },
  quick_auth_failed: { title: "识别码或密码错误", detail: "检查被控端显示的 9 位识别码与密码。" },
  internal: { title: "服务器内部错误", detail: "稍后重试；持续出现请查看服务器日志。" },
};

export type ErrorCopy = { title: string; detail: string };

export function describeCode(code: number, fallback = ""): ErrorCopy {
  const hit = CODES[code];
  if (hit) return hit;
  if (fallback) return { title: "操作失败", detail: fallback };
  return { title: "操作失败", detail: `错误码 ${code}` };
}

export function describeHttp(error: string, msg: string): ErrorCopy {
  const hit = HTTP_ERRORS[error];
  if (hit) return hit;
  return { title: "操作失败", detail: msg || error };
}

export function describeThrown(error: unknown): ErrorCopy {
  if (error instanceof Error) {
    if (/Failed to fetch|NetworkError|fetch failed/i.test(error.message)) {
      return { title: "连不上服务器", detail: "检查服务器地址与网络连接。" };
    }
    return { title: "操作失败", detail: error.message };
  }
  return { title: "操作失败", detail: String(error) };
}
