# 局域网直连

同网段的两台机器不经过账号服务器，直接建立控制会话。

## 为什么可以复用全部现有代码

中转会话的每一层都是传输无关的：

- `crates/agent/src/cloud.rs` 的 `session_task` 只依赖一个输入通道（`mpsc::UnboundedReceiver<Inner>`）
  和一个输出通道（`mpsc::UnboundedSender<WsMessage>`）。采集、输入注入、文件、音频、剪贴板
  全在里面，它不知道字节最终去哪儿。
- 服务器做的事只有一件：把 `{kind, session, payload}` 的 tunnel 帧按 `session` 号转给另一端
  （`crates/server/src/ws.rs` 的 `handle_view_binary`），不解码内容。
- 两端的帧格式由 `screenwall_proto::cloud::{encode_tunnel_frame, decode_tunnel_frame}` 定义。

所以直连**不需要新协议**：在被控端本地放一个「服务器桩」，把收到的 tunnel 帧直接交给
`session_task`，把 `session_task` 产出的帧直接发回主控端。协议、能力协商、授权、
自适应画质、文件传输、音频全部原样复用。

## 传输

- 被控端监听 `0.0.0.0:47900`，TLS（自签证书，`crates/agent/src/tls.rs` 已实现）。
- 主控端连 `wss://<被控端IP>:47900/ws`。
- 自签证书无法用 CA 验证，改为**固定指纹**：主控端首次连接时把证书 SHA-256 指纹存下来，
  之后每次连接比对，不一致就直接拒绝。**实际文案**：日志里是「直连证书指纹不匹配，已拒绝连接」，
  界面收到的是 TLS 握手失败被包成的一行 `无法连接 <地址>: …`（底层 rustls 错误原文含
  `unexpected error: 证书指纹不匹配`），没有单独做成「对方证书已更换」这种提示。
- 握手沿用 `Message::Hello` / `Message::Welcome`，token 是配对码（见下）。

## 配对码

被控端在界面上显示一个 6 位数字配对码，主控端输入它来完成首次连接。

- 配对码由被控端在「开启局域网直连」时生成，有效期 10 分钟。到期或被换掉后，
  对应的 token 立刻从白名单里失效（`allow_for` 带过期时刻，`is_allowed` 判过期），
  不是只在界面上不显示。
- 配对码派生成 token：`hex(SHA-256("screenwall-direct-pairing:v1:" || code))`。
  **必须是纯函数** —— 主控端只知道配对码，不知道被控端的任何秘密，
  两边要能各自算出同一个值。早期版本掺了被控端的 `direct_secret`，
  结果两边算出的值必然不同，连接一律被拒（已修正）。
- 安全性不靠这个哈希，靠三件事：配对码 10 分钟过期且用一次就换、
  被控端对认证失败限流、连接本身是 TLS（token 不在明文里传输）。
- 首次配对被授权通过后，被控端把该 token 从「配对 token」升成「长期 token」
  （`DirectAuth::promote`）；主控端保存 `(address, cert_fingerprint, token)`，
  之后免配对码直连，且每次连接都比对证书指纹。

### 认证失败限流

6 位配对码只有 100 万种可能，不限流就能被暴力枚举。`AuthLimiter` 按来源 IP 计数
（`crates/agent/src/direct.rs`）：

| 参数 | 值 | 含义 |
| --- | --- | --- |
| `AUTH_FAIL_LIMIT` | 5 | 连续失败达到这个次数才开始冷却 |
| `AUTH_FAIL_WINDOW` | 60 秒 | 距上次失败超过这么久，失败计数清零 |
| `AUTH_BACKOFF_CAP` | 30 秒 | 冷却上限（`2^n` 秒，n = 超出次数，封顶 30） |

- 计数在 `serve()` 里做：token 校验失败 `record_failure`，成功 `record_success` 清零。
- 被限流的 IP 在 accept 循环里直接 `drop(tcp)`，**连 TLS 握手都不做**，
  避免攻击者靠反复握手消耗 CPU（每次连接都 spawn 任务本身就是廉价 DoS）。
- 冷却到期就放行一次；再失败则计数上涨、冷却翻倍，到 30 秒封顶。这样既挡住爆破，也不会把正常用户永久锁在外面。

## 会话流程

```
主控端                                     被控端（本地桩）
  |-- TLS 连接 wss://ip:47900/ws ----------->|
  |-- Hello { proto, token, client, caps } ->|  校验 token + 证书指纹
  |<-- Welcome { proto, agent, limits, caps}-|
  |<-- DisplayList / Status -----------------|
  |                                          |
  |-- {t:"open", request, viewer} ---------->|  走 approval 回调，弹授权窗
  |<-- {t:"open.ok", session} ---------------|  分配本地 session 号
  |                                          |
  |-- tunnel 帧（session=N）---------------->|  直接喂给 session_task
  |<-- tunnel 帧（session=N）----------------|  session_task 的输出
```

`open` / `open.ok` / `open.err` / `close` 这四个控制消息的格式与服务器中转完全一致，
所以主控端 `handle_view_text` 里的处理逻辑不用改。

## 与中转模式的关系

| | 中转 | 直连 |
|---|---|---|
| 需要账号 | 是 | 否 |
| 需要服务器 | 是 | 否 |
| 跨网段 | 能 | 不能（被控端只接受局域网来源地址，公网来源直接断开） |
| 授权 | 逐次弹窗 | 首次弹窗；开启「已授权设备免确认」后，长期 token 不再弹窗 |
| 审计 | 写服务器 `audits` 表 | 不上报；本地日志只有连接/断开两条 `info`，无审计式记录 |
| 画质上限 | 受服务器带宽 | 受局域网带宽 |

两种模式可以同时开启：客户端既连服务器（被别人远程控、看设备列表），
又监听 47900（同网段的人可以直连进来）。

设备列表**不会**标出哪些设备可以直连（`DeviceView` 里没有网段/直连字段）。直连的唯一入口是
设备页工具条上的「直连」按钮：弹窗里手填对方地址与（可选的）配对码。

## 安全边界

直连绕过了账号服务器，所以这些必须做到：

1. **默认关闭**。设置 → 连接 → 局域网直连，用户显式打开才监听。
2. **只接受局域网来源**。端口绑 `0.0.0.0`（为了多网卡：有线、无线、虚拟网卡都能听），
   但 accept 时会校验来源地址：`crates/agent/src/direct.rs` 的 `is_lan_peer` 放行回环、
   RFC1918 私网、IPv4 链路本地 `169.254/16`、IPv6 回环 / 唯一本地 `fc00::/7` / 链路本地
   `fe80::/10`，以及 IPv6 内嵌 IPv4 映射地址（`::ffff:192.168.x.x` 先还原成 IPv4 再判）；
   其余一律当公网地址拒绝。被拒的连接直接 `drop(tcp)`，**连 TLS 握手都不做**，日志是
   「直连只接受局域网来源，已拒绝」。
   这个校验在限流检查之前，所以来自公网地址的爆破连计数都不会产生。
   **防火墙规则只放行 ScreenWall 自己**。安装包改为 `perMachine` 模式（安装时需要
   管理员权限），NSIS 安装钩子 `crates/client/windows/installer-hooks.nsh` 在文件落盘后
   调用 `netsh advfirewall` 添加一条入站 TCP 规则 `ScreenWall Direct`，`program=` 指向
   `$INSTDIR\screenwall-client.exe`，只对专用/域网络放行；卸载时删除同名规则。
   规则名与端口（默认 47900）必须在钩子和 `crates/client/src/firewall.rs` 之间保持一致。

   兜底路径：从旧的 `currentUser` 版本升级、规则被用户/组策略删掉、程序换过安装目录，
   或用户改过端口时，设置页会检测到规则缺失/不匹配并显示「允许局域网直连」按钮。
   点击后应用以管理员身份重跑自身的内置子命令（一次 UAC），先删同名规则再按当前
   exe 与端口重建。**不会**把端口对整个系统开放。
   如果用户拒绝 UAC，两台物理机会表现为「端口在听，但对方连不上」。排障见下面
   「连不上怎么查」。
3. **配对码一次性**。被控端在**授权通过**后立刻作废旧码并生成新码（`on_promote` → `rotate_pairing`）；
   只连上、没被允许的配对码不会作废（否则用户拒绝一次就得重新输码）。
4. **证书指纹固定**。首次连接记下指纹，之后不匹配直接拒绝。
5. **授权不可关闭**。直连的每次会话都必须有人在本机点「允许」，
   不受「允许被控」总开关影响 —— 那个开关只管中转会话。
6. **审计只在本机**。直连会话不上报服务器（服务器不知道这条会话存在），
   本地日志里只有被控端的两条 `info`：连接建立「局域网直连客户端已连接」、
   会话关闭「主控端关闭直连会话」；主控端侧只有 `warn` 级的异常记录（「直连会话中断」）。
   **没有**审计式的结构化记录（查看者、起止时间、时长、流量）。
   日志在 `%APPDATA%\ScreenWall\logs`。
7. **授权可撤销**。配对成功后的长期 token 存在 `direct_tokens`，
   设置页显示条数并提供「清除授权」，撤销后对方必须重新输配对码。
8. **凭据不出诊断包**。`knownPeers[].token` 与 `directTokens[]` 在导出诊断包时
   一律替换为 `<已脱敏>`。

## 连不上怎么查

直连是纯局域网 TCP + TLS，没有中转兜底，所以「连不上」的排查顺序固定：

1. **端口在不在听**：被控端 `netstat -ano | findstr 47900`，应看到 `0.0.0.0:47900 LISTENING`。
2. **防火墙放行没有**：`Get-NetFirewallRule -DisplayName 'ScreenWall*' | Get-NetFirewallPortFilter`
   看 `LocalPort` 是否含 47900、`Get-NetFirewallRule -DisplayName 'ScreenWall*' | Get-NetFirewallApplicationFilter`
   看 `Program` 是否指向当前安装目录的 `screenwall-client.exe`；`Profile` 若只有 `Public`，
   在「专用网络」下不会生效。perMachine 安装器会自动建这条规则；规则缺失或指向旧路径时，
   设置 → 连接 → 局域网直连 会出现「允许局域网直连」按钮，点一次即可（需要 UAC）。
   手动补一条（需要管理员）：
   ```powershell
   New-NetFirewallRule -DisplayName "ScreenWall Direct" -Direction Inbound -Protocol TCP -LocalPort 47900 -Program "C:\Program Files\ScreenWall\screenwall-client.exe" -Profile Private,Domain -Action Allow
   ```
3. **网段通不通**：被控端 `ping <主控端IP>`，主控端 `Test-NetConnection <被控端IP> -Port 47900`。
   后者 `TcpTestSucceeded: False` 而第 1 步在听，就是防火墙。
4. **配对码过期**：旧码到期即失效；监听期间 `keep_pairing_fresh` 会自动换新码并推 `direct-state`，
   界面会看到新码，不用手动点「重新生成」。如果对方还在用旧码，让他刷新界面重新输入。
5. **被限流**：同一 IP 连续失败 5 次会退避最多 30 秒，日志里有
   `直连认证失败次数过多，暂时拒绝`。等 30 秒或换一台机器试。
6. **来源不是局域网**：被控端只接受局域网来源地址。如果两台机器之间隔了路由器端口映射、
   或者主控端走公网 IP / 公网地址连过来，会在 accept 阶段就被断开，日志里有
   `直连只接受局域网来源，已拒绝`（连 TLS 握手都不做，所以表现为直接连不上）。

## 实现清单（已完成）

> **现状（2026-11-24 核对）**：本节仍准确，只有两处随代码演进而更新：`run()` 多了一个 `limiter: Arc<AuthLimiter>` 参数（见「实现契约」第 2 节），accept 循环在限流之前先做局域网来源校验（`is_lan_peer`，见「安全边界」第 2 条）。

1. `crates/agent/src/direct.rs`（新）：本地桩，把 tunnel 帧接到 `session_task`。
2. `crates/agent/src/cloud.rs`：`session_task` 相关接口提成 `pub(crate)`，
   `handle_control` 的 `open` 分支抽成可复用的 `open_session`。
3. `crates/client/src/direct.rs`（新）：客户端侧监听循环、配对、直连接入。
   命令 `get_direct_state` / `set_direct_enabled` / `regenerate_pairing_code` /
   `forget_direct_peers` / `connect_direct`。
4. `crates/client/src/main.rs`：`DirectLink` + `control_out()` 路由，
   让控制类帧能走直连通道；`Tasks.direct` 独立任务。
5. `ui/src/pages/SettingsPage.tsx`：直连开关、地址、端口、会话数、配对码、
   已授权设备数与「清除授权」。
6. `ui/src/pages/DevicesPage.tsx`：直连入口（输入 IP + 配对码）。
7. `ui/src/pages/ControlPage.tsx`：复用已有会话（直连设备不在设备列表里）。
8. 测试（截至本次核对，数量以代码为准）：`crates/agent/tests/direct.rs` 的集成用例覆盖
   端到端会话、`Welcome.caps` 是交集、无授权被拒、错误 token 被拒、拒绝授权补 `closed`、
   token 提升与整体替换、`promote_notifies_hook_once` 落盘通知不重复、`pairing_token_expires`
   配对码到期即拒、`revoke_pairing_keeps_promoted_token` 换码不误伤已授权设备、
   `auth_limiter_backs_off_after_repeated_failures` 失败限流与成功清零。
   `crates/agent/src/direct.rs` 的 `tests` 模块另有 6 个限流单测（未达上限不拦、不连坐其它 IP、
   成功清零、退避逐级增长且冷却后放行、冷却封顶 30 秒、窗口过期作废记录）
   和 2 个来源校验单测（`lan_peer_accepts_local_ranges` / `lan_peer_rejects_public_addresses`）。
   `crates/client/src/logging.rs` 的 `export_scrubs_peer_tokens` 覆盖两个方向的
   直连凭据都不进诊断包。
9. 文档：本文、`docs/protocol.md`、`README.md`、`docs/DELIVERY.md`。
10. 防火墙：`crates/client/windows/installer-hooks.nsh` 在 perMachine 安装/卸载时
    添加/删除 `ScreenWall Direct` 规则；`crates/client/src/firewall.rs` 做只读检测、
    内置提权子命令与设置页兜底按钮（命令 `get_direct_firewall_state` /
    `allow_direct_firewall`）。

## 实现中踩到的三个坑

记下来，免得以后重犯：

1. **会话级 `hello` 必须在拿到会话号后立刻发。** 中转路径里这步由
   `start_control_impl` 做，直连漏掉会表现为「会话建立了但一帧画面都没有」——
   被控端 `session_task` 等 5 秒收不到 hello 就超时。而且要在**等授权期间**
   就发出去（被控端的 pending 缓存会存住），不能等授权返回。
2. **`open.ok` 不等于授权通过。** 被控端先回 `open.ok`（分配会话号）再等人点
   「允许」，拒绝时补一条 `closed`。主控端不能拿到 `open.ok` 就当成功。
3. **配对码派生必须是纯函数。** 掺入任一方私密值都会导致两边算出的 token
   不同，连接一律被拒。

## 收口时修掉的八个问题

验收之后又过了一遍实现，发现八处会真的出事：

1. **白名单灌错了方向。** `listen_loop` 启动时把 `known_peers`（我作为主控端
   连出去用的凭据）灌进了「谁能连我」的白名单。后果是 A 直连过 B 之后，B 就能
   反过来免配对码连 A —— 信任关系被单向泄露成了双向。现在分成两个字段：
   `known_peers` 是「我能连谁」，`direct_tokens` 是「谁能连我」。
2. **长期 token 只活在内存里。** `DirectAuth::promote` 把配对 token 升成长期
   token 后没有落盘，被控端一重启就丢，主控端存的长期 token 再也连不上，
   「免配对码」只在单次运行内成立。现在 `DirectAuth::on_promote` 注册一个回调，
   客户端在回调里把 token 追加进 `direct_tokens` 并写配置。
3. **诊断包泄露直连凭据。** 导出诊断包只脱敏顶层 `token`，而
   `knownPeers[].token` 和 `directTokens[]` 都是能直接连上某台机器的凭据，
   会原样进包。现在三处一起脱敏，保留地址、设备名与条数。
4. **配对码熵不够。** 早先用「时间戳 ⊕ PID」拼伪随机，可预测。改用
   `getrandom` 取系统熵源。
5. **配对成功后没有换码。** 文档承诺「配对成功自动更换」，实际只把 token
   提升成长期凭据，配对码本身在 10 分钟内仍然有效 —— 谁拿到都能连。
   现在 `on_promote` 回调里调 `rotate_pairing`：撤掉配对集合里的旧 token
   （`revoke_pairing`，不碰长期集合），再生成新码并推 `direct-state`。
6. **配对码过期只是界面上的倒计时。** `DirectAuth` 里的配对 token 是永久
   放行的，10 分钟后界面不显示这个码了，但拿旧码的人照样能连进来。
   现在 `allow_for(token, ttl)` 带过期时刻，`is_allowed` 拒绝过期的；
   监听重建时按「配对码已经存在多久」算剩余时间，不会给旧码续命。
   另外 `keep_pairing_fresh` 在监听期间盯着到期时间，到点自动换码并推
   `direct-state`，界面不会卡在「已失效」等用户手动点「重新生成」。
7. **认证失败限流只写在文档里。** 文档说「被控端对认证失败限流」，代码里却
   根本没有 `AuthLimiter` —— 6 位配对码可以被无限次尝试，而且每次连接都 spawn
   一个任务，攻击者不用猜对码就能把进程拖垮。现在按来源 IP 计数：60 秒窗口内
   失败 5 次进入指数退避（封顶 30 秒），超限的 IP 在 accept 循环里直接
   `drop(tcp)`，连 TLS 握手都不做。
8. **退避算出来了但永不生效。** 第一版 `AuthLimiter` 记的是「**第一次**失败的
   时刻」，`blocked_for` 拿它算冷却 —— 结果一旦超过 5 次，整个 60 秒窗口内都
   恒定返回「还要等」，`2^n` 的指数退避永远升不到第二级，实际上退化成「锁 60 秒」。
   改成记「**最后一次**失败的时刻」：冷却到期就放行一次，再失败则计数上涨、
   冷却翻倍，到 30 秒封顶。补了 `limiter_backoff_grows_then_releases` 专门盯
   这条 —— 原来的测试只验证了「会拦」，没验证「会放行」。

另外补了两个界面缺口：设置页显示「已授权设备 N 台」，并提供「清除授权」
（二次确认后清空 `direct_tokens`，所有配对过的设备都要重新输码）。

---

# 实现契约

下面是三个模块之间的固定接口。**改动前先看这里**，三路并行开发都以此为准。

## 1. `crates/agent/src/cloud.rs` 需要暴露的内部接口

现在这些是私有的，直连要复用，必须提成 `pub(crate)`：

```rust
/// 一条服务器/直连连接上挂着所有会话的输入通道。
pub(crate) type Sessions = Arc<StdMutex<HashMap<u32, mpsc::UnboundedSender<Inner>>>>;

pub(crate) fn new_sessions() -> Sessions;
pub(crate) fn close_session(sessions: &Sessions, session: u32);

/// 把一段 tunnel 帧派发给对应会话。
pub(crate) fn dispatch(data: &[u8], sessions: &Sessions);

/// 从 `handle_control` 的 `"open"` 分支抽出来的会话启动逻辑：
/// 走授权 → 登记会话 → 跑 `session_task`。`handle_control` 和直连都调它。
pub(crate) async fn open_session(
    state: &Arc<AgentState>,
    session: u32,
    viewer: String,
    view_only: bool,
    out: &mpsc::UnboundedSender<WsMessage>,
    sessions: &Sessions,
);
```

`Inner` 保持私有，外部只用 `Sessions` 类型别名，这样调用方不需要知道 `Inner` 的存在。

## 2. `crates/agent/src/direct.rs`（新文件）

```rust
/// 直连白名单：当前允许连进来的 token 集合。
/// 客户端用配对码派生出短期 token 加进来，配对成功后换成长期 token。
#[derive(Default)]
pub struct DirectAuth { /* RwLock<HashSet<String>> */ }

impl DirectAuth {
    pub fn new() -> Self;
    pub async fn allow(&self, token: String);
    pub async fn revoke(&self, token: &str);
    /// 整体替换（配对码轮换时用）。
    pub async fn replace_all(&self, tokens: impl IntoIterator<Item = String>);
    /// 常量时间比较，防时序侧信道。
    pub async fn is_allowed(&self, token: &str) -> bool;
}

/// 监听直连连接。每条连接一个任务，错误只记日志不中断监听。
pub async fn run(
    listener: TcpListener,
    state: Arc<AgentState>,
    tls: TlsAcceptor,
    auth: Arc<DirectAuth>,
    limiter: Arc<AuthLimiter>,
) -> Result<()>;
```

单条连接的处理：

1. TLS 握手（10 秒超时）→ WebSocket 握手（10 秒超时）
2. 5 秒内必须收到 `Hello`；`proto` 不符回 `PROTO_UNSUPPORTED`
3. `auth.is_allowed(hello.token)` 为假 → 回 `AUTH_FAILED` 并断开
4. `state.approval` 为 `None` → 直接拒绝（直连不允许免授权）
5. 回 `Welcome` / `DisplayList` / `Status`（直接复用 `cloud.rs` 里已有的构造函数，
   需要的话把它们也提成 `pub(crate)`）
6. 主循环：
   - 文本帧 → `{"t":"open"}` 时**自己分配 session 号**（本地计数器，从 1 开始），
     调 `open_session(...)`；`{"t":"close"}` 调 `close_session`；
     `{"t":"ping"}` 回 `{"t":"pong","ts":...}`
   - 二进制帧 → `dispatch(data, &sessions)`
   - Ping → Pong；Close → 退出
7. 连接结束：`close_session` 掉本连接的所有会话

## 3. 客户端对前端的命令契约（`crates/client/src/main.rs`）

```ts
type DirectState = {
  enabled: boolean;        // 用户是否打开了直连（写进配置）
  listening: boolean;      // 实际是否在监听（可能因端口占用为 false）
  port: number;            // 默认 47900
  addresses: string[];     // 本机局域网 IPv4，界面直接显示给用户
  pairingCode: string | null; // 6 位数字；未开启时 null
  pairingExpiresIn: number;   // 剩余秒数；未开启时 0
  activeSessions: number;     // 当前直连进来的会话数
  trustedPeers: number;       // 已授权可直连本机的设备数
};

get_direct_state(): DirectState
set_direct_enabled(enabled: boolean): DirectState
regenerate_pairing_code(): DirectState
forget_direct_peers(): DirectState   // 清空「谁能连我」，都要重新输配对码

// 与 start_control 返回同样的 ControlInfo，控制页不用区分来源
connect_direct(input: { host: string; port?: number; pairingCode?: string }): ControlInfo
```

## 4. 配置新增字段（`ClientConfig`，`%APPDATA%\ScreenWall\client.json`）

```rust
#[serde(default)] direct_enabled: bool,        // 默认 false
#[serde(default = "default_direct_port")] direct_port: u16,  // 47900
#[serde(default)] known_peers: Vec<KnownPeer>, // 我能连谁
#[serde(default)] direct_tokens: Vec<String>,  // 谁能连我
```

两个方向必须分开存：`known_peers` 是我作为主控端连出去用的凭据，
`direct_tokens` 是我作为被控端放行的凭据。合成一个字段会让「A 连过 B」
变成「B 也能免码连 A」。

**没有 `direct_secret`。** 配对码派生必须是纯函数（见第 5 节），掺入任何一方私密值都会导致两边算出的 token 不同。

```rust
struct KnownPeer {
    address: String,      // "192.168.1.20:47900"
    fingerprint: String,  // 对方证书 SHA-256（大写冒号分隔，与 tls.rs 一致）
    token: String,        // 长期 token
    name: String,         // 对方设备名
}
```

## 4.5 配对码 → 长期 token 的升级（重要）

主控端首次用配对码连上后会存下那个 token 当长期 token。但那个 token 只在配对码
有效期内被放行 —— 配对码过期或用户换码后，主控端再连就会被拒。

所以 `DirectAuth` 需要区分两类 token：

- **配对 token**：`pairing.token`，随配对码轮换，作废即失效
- **长期 token**：配对成功后保留

做法：`DirectAuth` 加一个「授牌」标记。被控端在**授权通过**（`open` 被允许）时，
把这条连接用的 token 从「配对 token」升级成「长期 token」：

```rust
impl DirectAuth {
    /// 把一次性的配对 token 升成长期 token（授权通过时调用）。
    pub async fn promote(&self, token: &str);
    /// 长期 token 是否还有效（重启后从配置恢复用）。
    pub async fn is_promoted(&self, token: &str) -> bool;
    /// 当前长期 token 条数，界面显示「已授权 N 台」。
    pub async fn promoted_count(&self) -> usize;
    /// 注册落盘回调：新增一个长期 token 时调用，宿主据此写进 `direct_tokens`。
    pub fn on_promote(&self, callback: PromoteHook);
}
```

`run` 里在 `open_session` 前（或授权回调返回 true 后）调 `auth.promote(&hello.token)`。
**提升必须落盘**：只存在内存里的话被控端一重启就丢，主控端存的长期 token
再也连不上，「免配对码」只在单次运行内成立。客户端通过 `on_promote` 回调把它
追加进 `direct_tokens` 并写配置；下次启动时 `listen_loop` 用 `replace_all`
把这份清单灌回白名单。

注意方向：被控端落盘的是 `direct_tokens`（谁能连我），主控端存的是
`known_peers`（我能连谁）。两个方向不能混用。

## 5. 配对码派生

```
pairing_token = hex(SHA-256("screenwall-direct-pairing:v1:" || pairing_code))
```

- `pairing_code` 是 6 位数字（`000000`–`999999`），由 `getrandom` 生成。
- 有效期 10 分钟，`regenerate_pairing_code` 或到期后作废。
- 配对成功（第一次 `open` 被允许）后，被控端把 `pairing_token` 换成长期 token
  加进 `DirectAuth`，并把配对码作废。

## 6. 前端文件边界

- `ui/src/api.ts`：加 5 个函数 + `DirectState` 类型，非 Tauri 演示模式返回假数据。
- `ui/src/types.ts`：加 `DirectState`。
- `ui/src/pages/SettingsPage.tsx` + `.css`：连接区块加「局域网直连」小节。
- `ui/src/pages/DevicesPage.tsx` + `.css`：工具条加「直连」按钮 → 弹窗填 IP + 配对码。

## 7. 任务结构（重要）

**直连监听不能放在 `host_loop` 里。** `host_loop`（`crates/client/src/main.rs`）开头就是
`if config.token.is_empty() || config.server_url.is_empty() { return; }`
—— 没登录就退出。而直连的核心价值恰恰是不需要账号，
所以必须是一条独立任务：

- `spawn_tasks`（`crates/client/src/main.rs`）里增加
  `direct::listen_loop(app, shared)` 并 spawn，存进 `Tasks.direct`。
- `stop_tasks` / `restart_tasks` 也要处理这个句柄。
- `listen_loop` 自己循环：读配置 → 若 `direct_enabled` 为假就等待后继续
  （用户可能在设置里随时打开）→ 否则绑定端口并 `direct::run(...)`，出错退避重试。

> 行号会随改动漂移，这里只保留函数名，具体位置以代码为准。

## 8. TLS 接受器的获取方式

客户端不要直接依赖 `rustls` / `tokio-rustls`。在 `crates/agent/src/tls.rs` 增加：

```rust
/// 一行拿到可直接用于 `run_server` / `direct::run` 的 TLS 接受器。
pub fn ensure_acceptor(dir: &Path) -> Result<(TlsAcceptor, String)>;
// 返回 (acceptor, fingerprint)
```

`crates/client` 已经依赖 `screenwall-agent`，直接调用即可。

## 9. 主控端输出路由

`shared.view_tx` 目前是**唯一**的控制输出通道（`files.rs`、`main.rs` 的控制路径都在用）。
直连会话需要把帧发到直连 WebSocket 而不是服务器。做法：

```rust
// main.rs
struct DirectLink {
    tx: mpsc::UnboundedSender<ViewOut>,
    session: u32,
    address: String,
}

// Shared 新增
direct: RwLock<Option<DirectLink>>,

/// 控制会话的输出去向：直连会话走直连通道，其余走服务器。
async fn control_out(shared: &Shared, session: u32) -> Option<mpsc::UnboundedSender<ViewOut>> {
    if let Some(link) = shared.direct.read().await.as_ref() {
        if link.session == session {
            return Some(link.tx.clone());
        }
    }
    shared.view_tx.read().await.clone()
}
```

**只替换控制类调用点**（`files.rs` 的两处、`main.rs` 里 `send_tunnel_json` / `send_control_input` /
`set_stream_options` / `set_audio_enabled` 等控制路径）。`wall.rs` 与 `group.rs` 保持用
`view_tx` 不动 —— 屏幕墙和群控是账号功能，直连没有对应语义。

## 本机模式（不用服务器）

登录页的「进入局域网模式」把客户端切成纯直连形态，配置里是 `local_mode: true`：

- 不连 `/ws/view`、`/ws/device`，`spawn_tasks` 只拉起直连监听和画质自适应；
- 自动打开直连监听并生成配对码，用户不必再进设置点开关；
- 设备列表来自 `known_peers`（配对过的直连设备），屏幕墙显示的就是这些机器；
- 屏幕墙不列本机，也不给本机开缩略图会话 —— 本机是操作台，不是被看的对象。
  这条在两种模式下都成立：`WallPage` 过滤掉 `account.deviceId`，
  `wall::set_preview` 在 Rust 侧再剔除一次，防止换调用方时漏掉。

退出本机模式（侧栏登出按钮）会回到登录页，已配对的直连设备保留。


`handle_view_binary` / `handle_view_text` 不改：直连的 WebSocket 读循环直接调用它们，
按 `header.session` 分流本来就能工作。
