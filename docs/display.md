# 显示器关闭、无头环境与虚拟屏 / 超级屏调研

> 状态：调研文档（2026-11-24）
> 适用：ScreenWall v0.1，Windows 10/11 x64，被控端 GDI 采集
> 范围：只讨论被控端显示输出与采集，不涉及编码、网络和输入协议细节
> 结论标注：✅ 官方文档/可复现；⚠️ 本机实测/社区共识；❓ 推测，需在目标机器验证

## 0. 结论摘要

一句话：**显示器“关掉”不等于“没有显示器”。Windows 电源计划关屏（DPMS）通常还能采到画面；物理断电、拔线、无头开机则不保证，真正无头要靠虚拟显示器驱动（IDD）或 HDMI/DP 假负载。**

- ✅ **Windows 电源计划关屏 / DPMS**：系统仍认为显示器存在，桌面仍由 DWM 合成。本项目在本机 1920×1080 上用 `SC_MONITORPOWER` 关屏后重新抓屏，画面均值/方差几乎不变，`RESULT: PASS`（见 `docs/m3.md` 和 `scripts/test-monitor-off.ps1`）。GDI `GetDC(0)` + `BitBlt/StretchBlt` 可用；DXGI Desktop Duplication 通常仍能复制，但没有新桌面更新时会大量返回 `DXGI_ERROR_WAIT_TIMEOUT`，不代表黑屏。
- ⚠️ **显示器电源键关闭但线接着**：结果取决于接口和显示器。HDMI 的 EDID EEPROM 常由源端 5V 供电，显示器“关了”仍可能被 Windows 识别；DisplayPort 的显示器在断电后可能连 HPD/EDID 一起掉，Windows 会当成拔线，桌面重排。此时 GDI 可能只能采到剩余显示器；如果一块都不剩，就可能黑屏或 `StretchBlt` 失败。
- ⚠️ **完全没接显示器开机 / 机房无头 / 笔记本合盖睡眠**：没有活动显示输出时，不能指望 GDI 或 DXGI 变出画面。Windows 可能回退到无 EDID 的 1024×768 模式，也可能根本没有可用输出；社区大量报告黑屏。需要 IDD 虚拟显示器或假负载，另外还要自动登录 + 托盘进程自启，否则连登录界面都采不到。
- ✅ **`SetThreadExecutionState` 只能解决“空闲超时”这一类**：它重置显示器/系统空闲计时器，防止电源计划自动关屏或睡眠；不能阻止用户按电源键、合盖、拔线，也不会停止屏保。它不是虚拟显示器。本项目已在采集线程中调用 `ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED`，线程退出时恢复。
- ✅ **纯软件“多屏拼接”能抓整块虚拟桌面，但不是新显示器**：用 `SM_XVIRTUALSCREEN` / `SM_YVIRTUALSCREEN` / `SM_CXVIRTUALSCREEN` / `SM_CYVIRTUALSCREEN` 取所有显示器的包围盒，`GetDC(0)` + `StretchBlt` 一次抓出整张画面。它零依赖、无需管理员，但只对“已经存在的活动显示器”有效；OS 和应用仍把每块屏当独立显示器，不能突破物理分辨率，也不能创造无头输出。本项目当前实现就是这条路。
- ✅ **真正的“超级屏”/虚拟屏是 IDD**：网易 UU 远程的“超级屏”官方定义就是虚拟显示方案（最高 4K/144，合并所有物理/虚拟屏到一个窗口，使用期间物理屏关闭）；ToDesk 的“虚拟屏”也是虚拟显示器，最高 5120×1440、最多 3 块，要求 Win10 1903+。这类方案能在无头机器上工作，但需要安装驱动，涉及签名、管理员权限、卸载和兼容性。
- **推荐路线**：短期保持“虚拟桌面拼接 + 保活 + 检测降级”，无头机器推荐 HDMI/DP 假负载；中期只做虚拟显示器检测和用户引导，不静默装驱动；长期若必须“单安装包、开箱即用无头”，再评估自带签名 IDD，工作量和维护成本都明显更高。

---

## 1. 显示器关闭的几种情况

先区分四件事，它们对采集的影响完全不同：

1. **显示器进入低功耗/关屏，但显示链路还在**（Windows 电源计划、`SC_MONITORPOWER`、显示器待机）。
2. **显示器被物理断电或拔线，Windows 认为显示器消失**（DisplayPort 尤其容易发生）。
3. **开机时就没有显示器**（无头、机房、迷你主机）。
4. **笔记本合盖导致睡眠/关屏/切换显示输出**。

### 1.1 总表

| 场景 | Windows 是否还认为有活动显示器 | GDI `GetDC(0)` + BitBlt/StretchBlt | DXGI Desktop Duplication | 分辨率 | 结论 |
|---|---|---|---|---|---|
| 电源计划关屏 / DPMS | 通常是 | ⚠️ 本机实测可用；DWM 仍在合成 | 通常可复制；无更新时 `WAIT_TIMEOUT`；输出被移除时 `ACCESS_LOST` | 不变 | 最乐观，保活可预防 |
| 显示器电源键关，HDMI 线接着 | 多数情况下是 | ⚠️ 通常可用，取决于显示器是否还保持 HPD/EDID | 同左；无新帧时超时 | 不变 | 逐台实测 |
| 显示器电源键关，DP 线接着 | 可能否 | 若 Windows 仍识别则可用；若 HPD 掉了则黑/失败 | 输出移除后无法继续，`ACCESS_LOST` | 不变或桌面重排 | DP 更容易掉链路 |
| 直接拔线 | 否（该显示器） | 剩余显示器可采；一块不剩则黑/失败 | 该输出消失，需重建；无输出则无法复制 | 桌面重排 | 不能靠软件保活 |
| 无显示器开机 / 无头 | 通常否，或只有 no-EDID 回退 | 常见黑屏、低分辨率或失败 | 无活动输出时无法建立；有回退输出时也可能黑 | 1024×768、800×600 或不可用 | 需 IDD/假负载 |
| 笔记本合盖 | 取决于合盖动作和固件 | 若只是关内屏但桌面仍活动则可用；睡眠则停 | 同左 | 内屏关闭/外屏分辨率 | 先改电源动作 |
| RDP 会话断开 / 锁屏 | 会话断开或安全桌面 | 可能黑、空会话或失败 | `DXGI_ERROR_SESSION_DISCONNECTED` / `ACCESS_LOST` | RDP 会话分辨率 | 不要把 RDP 当无头方案 |

> “显示器电源键关闭”与“Windows 电源计划关屏”不是一回事。前者是显示器自己的硬件行为，可能直接切断 HPD/EDID；后者是操作系统请求显示器进入低功耗，显示链路通常仍在。

### 1.2 GDI 采到的到底是什么

✅ 官方文档：`GetDC(NULL)` 返回“整个屏幕”的 DC（[GetDC](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getdc)）。GDI 的 `BitBlt` / `StretchBlt` 从屏幕 DC 读取的是**桌面合成后的表面**，不是从显示器线缆里“抓电信号”。

因此判断 GDI 能不能采，关键不是“显示器亮不亮”，而是：

1. 这个会话里是否还有**活动的桌面输出**；
2. DWM/GPU 是否还在维护可读的桌面表面；
3. 显卡驱动是否因为无显示器/低功耗而停止输出或把表面置黑；
4. 进程是否在**交互式用户会话**里（Windows 服务在 session 0，采不到用户桌面）。

这解释了两类看似矛盾的现象：

- 显示器通过 DPMS 关掉，但 Windows 仍认为显示器存在，桌面继续合成：GDI 往往还能采到，本项目本机实测就是这种情况。
- 显示链路被移除（拔线、DP 断电导致 HPD 掉、无头开机），Windows 没有活动输出：GDI 可能返回全黑、旧画面或直接失败。社区和远程软件厂商都报告过这种黑屏。

⚠️ 注意：GDI 黑屏不一定是“显示器关了”。DRM/受保护内容、全屏独占、硬件覆盖层、UAC 安全桌面、锁屏、RDP 会话等也会黑。检测时要组合多个信号，不能只看一帧是不是黑的。

### 1.3 DXGI Desktop Duplication 的差异

✅ 官方文档要点：

- Desktop Duplication API 按**活动输出**复制桌面；要复制整个桌面，必须对每个活动输出各建一个 `IDXGIOutputDuplication`，然后自己按时间戳/坐标合并。官方没有提供跨输出同步（[Desktop Duplication API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)、[DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput)）。
- `AcquireNextFrame` 的成功/失败语义：
  - `DXGI_ERROR_WAIT_TIMEOUT`：超时前没有新的桌面帧。桌面没人操作时很常见，**不是错误**。
  - `DXGI_ERROR_ACCESS_LOST`：桌面切换、模式变化、DWM 开关、全屏应用等导致接口失效，需要释放并重建。
  - `DXGI_ERROR_INVALID_CALL`：上一帧没 `ReleaseFrame`。
  - 其他错误见 [AcquireNextFrame](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe)。
- `DuplicateOutput` 在会话断开时返回 `DXGI_ERROR_SESSION_DISCONNECTED`；同一会话默认最多 4 个进程同时复制。
- 如果应用要复制整个桌面，必须为每个活动输出建一个复制接口；没有活动输出时，就没有可复制的 output（这是 API 结构的直接推论，❓ 具体错误码取决于调用位置）。

因此：

- **DPMS 关屏但输出还在**：`DuplicateOutput` 可能仍然成功；如果桌面没有更新，`AcquireNextFrame` 大量返回 `WAIT_TIMEOUT`，你可以继续显示最后一帧。部分驱动/场景可能返回全黑或 `ACCESS_LOST`，⚠️ 需实测。
- **显示器被移除/无头**：该 output 不再活动，继续调用通常 `ACCESS_LOST`；重新枚举也找不到输出。DXGI 不会凭空生成虚拟显示器。
- **多屏**：DXGI 不能一次给你“整块虚拟桌面”，必须每屏一路再拼接；GDI 的 `GetDC(0)` + 包围盒反而更简单。

Windows Graphics Capture（WGC）也依赖系统合成/活动显示，官方没有承诺无显示器时可用；社区报告在无头/关屏时同样会黑。它适合做现代窗口/显示器采集，但**不是无头方案**。❓ 具体行为按 Windows 版本和 GPU 驱动实测。

### 1.4 分辨率会怎么变

- **DPMS/电源计划关屏**：分辨率不变。
- **显示器电源键关但链路还在**：分辨率不变。
- **拔线/DP 断电导致 HPD 掉**：Windows 移除该显示器，桌面重排；如果只剩一块，虚拟桌面尺寸变成剩余那块；一块不剩则没有有效桌面。
- **无头开机**：常见两种结果：
  - Windows/显卡驱动建立一个无 EDID 的回退输出，常见 1024×768（甚至 800×600），GDI 可能能采到但画面是黑/低分辨率；
  - 完全没有活动输出，GDI/DXGI 都拿不到有效画面。
  社区对 1024×768 回退的讨论和注册表 `NOEDID` 偏方见 [SuperUser: headless Windows 10 1024×768](https://superuser.com/q/1726706)。❓ 具体回退行为随 GPU/驱动/BIOS 不同，注册表偏方不稳定，不建议作为产品方案。
- **笔记本合盖**：如果合盖动作是“睡眠”，采集线程和网络都停；如果是“不采取任何操作”，可能仍保留桌面，但内屏可能被固件关闭。外接屏存在时通常以外接屏为准。
- **旋转屏**：GDI 采到的是旋转后的桌面布局；DXGI 的 surface 可能是未旋转的，旋转信息在元数据里（官方 Desktop Duplication 文档有说明）。多屏拼接要把旋转方向算进去。

### 1.5 本机已验证与未验证

✅ 已验证（本机，2026-11-24，1920×1080）：

- 用 `SendMessage(HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2)` 关屏，等待 3 秒，GDI `CopyFromScreen` 重新抓屏；
- 关屏前 `mean=152.47 stddev=99.86`，关屏后 `mean=152.56 stddev=99.85`，平均像素差 `0.41/255`；
- 结论：**DPMS 关屏、主机仍运行时，Windows 仍在合成桌面，GDI 采集可用**。
- 脚本：`scripts/test-monitor-off.ps1`；记录：`docs/m3.md` 第 3 节。

⚠️ 未验证（必须实机验收）：

- 显示器电源键物理关闭（HDMI 和 DP 分别测）；
- 拔掉视频线；
- 无显示器开机；
- 笔记本合盖（睡眠/仅关屏/外接屏）；
- 不同 GPU（Intel/AMD/NVIDIA）、不同驱动版本；
- 多屏热插拔后不重连采集线程的行为；
- DXGI 在上述场景的具体返回值（本文的 DXGI 关屏行为是 API 语义 + 社区报告，不是本机实测）。

---

## 2. 软件层保活：`SetThreadExecutionState` 能解决什么

### 2.1 官方语义

✅ 官方文档（[SetThreadExecutionState](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setthreadexecutionstate)）：

- `ES_CONTINUOUS`：状态持续有效，直到下一次带 `ES_CONTINUOUS` 的调用清除其他标志。
- `ES_DISPLAY_REQUIRED`：重置**显示器空闲计时器**，让显示器保持开启。
- `ES_SYSTEM_REQUIRED`：重置**系统空闲计时器**，让系统保持工作状态。
- 组合使用：`ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED` 可同时阻止“空闲关屏”和“空闲睡眠”。
- 状态是**线程级**的；不带 `ES_CONTINUOUS` 时需要周期调用重置。
- 官方明确：**不能阻止用户主动让电脑睡眠**，应用应尊重用户合盖/按电源键的预期；**也不能停止屏保**。
- `ES_AWAYMODE_REQUIRED` 只适合媒体录制/分发等特殊场景，不建议远程桌面使用。

### 2.2 它能解决哪一类

| 问题 | `SetThreadExecutionState` 是否有效 | 说明 |
|---|---|---|
| 电源计划“X 分钟后关闭显示器” | ✅ 有效 | `ES_DISPLAY_REQUIRED` |
| 电源计划“X 分钟后睡眠” | ✅ 有效 | `ES_SYSTEM_REQUIRED` |
| 屏保启动/锁屏 | ❌ 无效 | 官方明确不阻止屏保；锁屏是安全桌面 |
| 用户按显示器电源键 | ❌ 无效 | 这是显示器硬件行为 |
| 用户按主机电源键/合盖睡眠 | ❌ 无效 | 官方明确不能阻止用户主动睡眠 |
| 拔线/DP 断电导致显示器消失 | ❌ 无效 | 没有显示输出，保活不能变出 output |
| 无头开机 | ❌ 无效 | 需要 IDD 或假负载 |
| 组策略强制电源设置 | ⚠️ 不保证 | 没有看到官方“可覆盖 GPO”的承诺；按部署环境实测 |

### 2.3 本项目现状

✅ 当前 `crates/agent/src/capture.rs` 已经在采集线程启动时调用：

```text
SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED)
```

线程退出时调用 `SetThreadExecutionState(ES_CONTINUOUS)` 清除。作用域是采集线程，只有采集线程存活期间有效；停止订阅后恢复系统原来的电源行为。

这个实现是合理的，但要注意：

- 只能在**有采集线程**时保活。控制页关掉、会话结束、进程退出后，系统可以按电源计划关屏/睡眠。
- 进程被挂起/崩溃时，线程状态随之消失；不要把它当“永久保活”。
- 它不能阻止用户手动关显示器、合盖或拔线。
- 它不能解决无头/虚拟显示问题。
- 对组策略/第三方电源管理软件的效果需要在目标环境用 `powercfg /requests` 验证。

### 2.4 电源计划、组策略与无人值守

可以在被控端做的设置（需要管理员/用户确认）：

```powershell
# 交流电下：显示器从不关闭、系统从不睡眠（0 表示从不）
powercfg /change monitor-timeout-ac 0
powercfg /change standby-timeout-ac 0
# 查看当前有哪些应用/驱动持有电源请求
powercfg /requests
```

✅ `powercfg /change` 的 `monitor-timeout-ac` / `standby-timeout-ac` 参数和“值以分钟计”见 [Powercfg 命令行选项](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/powercfg-command-line-options)。0 表示从不，是常见用法；❓ 具体策略是否被 OEM/组策略覆盖需实测。

无人值守还需要：

1. **自动登录**：项目是托盘进程，不是服务；没有用户登录就没有交互式桌面。登录界面/锁屏/UAC 安全桌面不在 v1 非目标里，不能靠采集解决。
2. **托盘进程自启**：登录后自动启动；否则远程看到的是登录界面或黑屏。
3. **不要锁屏/屏保**：`SetThreadExecutionState` 不管屏保；企业策略可能强制锁屏。
4. **Windows Update/重启**：更新重启后需要自动登录和自启；WoL 只能开机，不能替你登录。
5. **笔记本合盖动作**：设为“不采取任何操作”，并确认固件不会强制关内屏；能不用笔记本做无头服务器就不用。
6. **远程管理通道**：无头机器如果显示/网络挂了，至少要有带外管理（IPMI、智能插座、同网 helper）作为退路。

### 2.5 软件保活的局限（写进用户文档）

- 只防“空闲超时”，不防“人为关屏/合盖/拔线/无头”。
- 不能创造显示器，不能提高分辨率，不能绕过物理链路。
- 不能停止屏保/锁屏。
- 不能保证覆盖组策略。
- 采集线程停止即失效。
- 对本项目来说，它是“减少黑屏概率”的措施，不是“无头方案”。

---

## 3. 虚拟显示器驱动（IDD / Indirect Display Driver）

### 3.1 原理

✅ 官方文档（[Indirect display driver overview](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)）：

- IDD 是**用户态 UMDF 驱动**，运行在 session 0，不需要内核态组件；驱动崩溃不会直接蓝屏。
- 通过 IddCx 创建“代表间接显示设备的图形适配器”，向系统报告显示器连接/断开、显示器描述、可用显示模式，并处理桌面图像。
- 它创建的虚拟显示器会出现在 Windows 显示设置里，可以像物理显示器一样设置分辨率、刷新率、排列、扩展/复制，参与桌面合成。
- 典型用途就包括：远程桌面/串流、虚拟桌面环境、USB 转 VGA/DVI/HDMI/DP 的 dongle。
- IDD 不是“采集 API”。它负责让 Windows 多一个输出；我们的程序仍然通过 GDI/DXGI 去采桌面。虚拟显示器的存在会迫使 Windows 维护对应的桌面输出，因此在无头机器上能让 GDI 有东西可采。
- IddCx 最低 Windows 10 1709；IddCx 1.4 对应 Windows 10 1903；更高版本见官方 IddCx 版本表。VDD 项目自述使用 IddCx 1.10，目标 Win10/11。

✅ 虚拟显示器可以设置自定义分辨率/刷新率/EDID；这也是 ToDesk 能提供 5120×1440、UU 能提供 4K/144 的原因。

### 3.2 可选项与许可证

| 方案 | 类型/许可证 | 签名/安装 | Windows | 能力与限制 | 对本项目 |
|---|---|---|---|---|---|
| **Microsoft IddSampleDriver**（Windows-driver-samples / IndirectDisplay） | 示例驱动，仓库 **MS-PL** | 默认**未签名**；安装需自行签名或开启测试签名（Intel 的 IDD readme 明确写了这点） | Win10 1709+（示例 IddCx 1.4，1903+ 更合适） | 只枚举一个显示器，代码里大量 TODO；不是生产驱动 | 作为学习/自研参考，不直接打包 |
| **Virtual Display Driver（VDD）** `VirtualDrivers/Virtual-Display-Driver`（原 itsmikethetech） | 开源 **MIT** | 项目 release 提供 **已签名**驱动和 VDC 安装器；也支持 `winget install --id=VirtualDrivers.Virtual-Display-Driver -e`；手动 INF/脚本见 Wiki | 自述 Win10/11；HDR 需 Win11 23H2+；ARM64 24H2+ 可能需测试签名 | 自定义分辨率/刷新率/EDID，HDR，多显示器；GPU 驱动大版本更新前建议先卸载 VDD，否则可能黑屏/显示优先级错乱 | 中期最现实的“用户自行安装”选项；是否允许随包再分发要核实签名/许可 |
| **usbmmidd v2**（Amyuni） | 免费软件，**非开源**；License 允许任何用途但要求保留声明/致谢，并会弹出广告页；商业版去广告 | 官方 zip 内有 `deviceinstaller64`、INF、CAT；说明称驱动经 Microsoft 签名流程，签名者为 Amyuni；安装必须管理员 | 官方说明写 **Windows 10 Only**（Win11 未官方承诺） | 最多 4 个虚拟显示器；默认 1920×1080；支持 1024×768 到 3840×2160 等固定列表；通过 `deviceinstaller64 enableidd 1` 启用 | 可作为用户手动方案，但不适合静默打包（广告/许可/仅 Win10） |
| **Parsec VDD** | 闭源，随 Parsec 分发 | 由 Parsec 安装器安装/签名 | 官方支持页要求 **Win10 21H2+ / Server 2019+** | 最多 3 个虚拟显示器；**不能创建持久虚拟显示器**，只在 Parsec 会话期间存在；虚拟显示是 Teams/Warp 功能 | 不适合作为本项目的通用依赖；也不适合再分发 |
| **ToDesk 虚拟屏** | 商业闭源 | 随 ToDesk 客户端；被控端需登录/授权 | 官方要求 **Win10 1903+** | 最多 3 个虚拟屏，最高 5120×1440；可配合关闭物理显示器（仅性能版）；远程会话中开启 | 用户已有 ToDesk 时可作为外部方案，不可嵌入 |
| **网易 UU 远程“超级屏”** | 商业闭源 | 随 UU 远程；驱动安装异常错误码 301 提示需 Win10+ | Win10+ | 虚拟显示方案，最高 4K/144；把所有物理/虚拟屏合并为一个超级屏窗口；使用期间物理屏会关闭；官方称 VIP 权益 | 产品形态参考，不可嵌入 |
| **向日葵虚拟屏/合屏** | 商业闭源 | 随向日葵客户端 | 官方帮助未给统一最低版本；合屏/分屏要求 Windows 个人版 V13.2+ 和会员 | “虚拟屏”勾选用于无显示器/关屏黑屏；“合屏/分屏/单屏”是查看方式；合屏是把多屏画面放进一个窗口，官方未说明是否创建虚拟显示器 | 参考“虚拟屏开关 + 合屏”的产品设计 |

许可证结论：

- **MIT 的 VDD** 是最适合“研究/可选集成”的开源方案；但 MIT 只覆盖代码，**已签名二进制能否随你的安装包再分发、是否需要保留签名者的声明**，要单独核实 release/文档，不要默认可以。
- **MS-PL 的 IddSampleDriver** 是示例，不是成品；直接打包会带来“未签名/测试模式/无技术支持”的问题。
- **usbmmidd** 许可允许使用，但有广告页和致谢要求，且官方只写 Win10；不适合作为单安装包里的默认驱动。
- **Parsec/ToDesk/UU/向日葵** 都是闭源商业软件，不能把它们的驱动抽出来随本项目分发。

### 3.3 驱动签名：必须区分“内核驱动”和“UMDF IDD”

这是最容易写错的一点。

✅ 官方事实：

- 从 Windows 10 1607 起，Windows 不加载**新的、未经 Dev Portal 签名**的内核模式驱动；注册 Hardware Dev Center 需要 **EV 代码签名证书**（[Driver Signing Policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)）。
- 向 Dashboard 提交驱动时，EV 证书是建立账户/提交的要求之一（[Driver code signing requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs)）。
- 可选 **attestation signing**（不要求 HLK）或 **WHQL/HLK**；官方 attestation 文档说明它主要面向测试/受控场景，零售/WU 发布仍应走 WHQL/HLK（[Attestation sign Windows drivers](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/attestation-signing-a-kernel-driver-for-public-release)）。具体政策以 Partner Center 当前说明为准。
- 测试签名需要开启 test signing，桌面会显示测试模式水印；在启用 Secure Boot 的机器上通常还要关闭 Secure Boot 才能加载测试签名驱动。**测试签名不适合分发给普通用户**（[Test Signing](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/test-signing)）。

✅ 但 IDD 是 **UMDF 用户态驱动**，不是内核驱动。微软官方旧文明确：UMDF 驱动不需要 Code Integrity 的内核签名，但**驱动包仍然需要签名**（catalog/INF），否则安装会有未签名提示甚至被阻止（[Do UMDF drivers require signing?](https://learn.microsoft.com/en-us/archive/blogs/peterwie/do-umdf-drivers-require-signing)）。

因此，准确的表述是：

- **“Win10 1607+ 新内核驱动必须 EV + Dev Portal 签名”**：对内核驱动成立。
- **“IDD 必须 EV + attestation”**：不完全准确。IDD 是 UMDF，技术门槛可能只是“驱动包有受信任的签名”；VDD 项目就声称其 release 由 SignPath 提供签名，并不等同于 Microsoft attestation。
- **“正式分发给普通用户仍然应该走 Microsoft 签名/正式发布路径”**：成立。即使技术上能装，未签名/自签名/测试签名会触发 UAC/驱动安装警告，企业环境可能直接阻止；要长期维护、兼容 Windows 更新，正式签名/WHQL 仍是稳妥路线。
- **“测试签名不适合正式分发”**：完全成立。

结论：不要用“反正是 UMDF 所以不用签名”来省成本；也不要对用户承诺“EV + attestation 一定够了”。在决定自研/打包驱动前，先向 Microsoft Partner Center 或签名服务商确认目标 Windows 版本、分发方式和所需签名类型。

### 3.4 安装、卸载与打包进 Tauri

驱动安装不是复制一个 exe：

- 通用安装：管理员权限下 `pnputil /add-driver <file.inf> /install`；查看已安装驱动包 `pnputil /enum-drivers`；卸载 `pnputil /delete-driver <oem#.inf> /uninstall /force`。✅ 官方 [PnPUtil 命令语法](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/pnputil-command-syntax)。
- 有些 root-enumerated 虚拟设备需要厂商安装器（VDD 的 VDC、usbmmidd 的 `deviceinstaller64`、IddSample 的 devcon 脚本）来创建设备节点；`pnputil /add-driver` 只把包放进驱动商店，不一定创建设备。具体以方案文档为准。
- 安装/卸载通常需要管理员；可能需要重启；卸载后要确认驱动商店、设备节点、残留的虚拟显示器都已清理。
- 驱动和 GPU 驱动更新可能冲突。VDD README 明确警告：大版本 GPU/芯片组驱动更新前先卸载 VDD，否则可能黑屏或显示优先级错乱；卡在黑屏时进安全模式卸载。这是无头机器上很现实的支持成本。
- Windows Update 可能替换/回滚驱动；企业安全软件可能拦截未签名/未知驱动。

打包进 Tauri：

- ✅ Tauri v2 的 NSIS `installMode: "perMachine"` 会安装到 `C:\Program Files`，需要管理员权限；默认 `currentUser` 不需要管理员（[Tauri Windows Installer](https://v2.tauri.app/distribute/windows-installer/)）。
- 驱动安装需要单独的提权步骤（自定义 NSIS 钩子/安装器），不能假设 Tauri 安装器会自动处理。
- 卸载要提供“删除驱动 + 设备节点 + 虚拟显示器 + 重启提示”的完整流程；只删应用目录会留下驱动。
- 安装器本身和驱动包通常要分别签名；UAC、SmartScreen、企业白名单都要考虑。
- 不要静默安装驱动；至少要有明确的用户同意、许可证展示和回滚路径。

### 3.5 对本项目的建议

**短期（现在）**

- 不打包任何驱动。
- 保持当前“虚拟桌面拼接 + `SetThreadExecutionState` + 检测/提示”。
- 在用户文档里明确：无头/关屏黑屏优先用 **HDMI/DP 假负载**；需要改分辨率/多虚拟屏再考虑 IDD。
- 检测到“没有活动显示器/长时间黑屏”时，给出可执行的提示，而不是静默黑屏。

**中期（可选）**

- 检测系统里是否已有虚拟显示器（见第 6 节），识别 VDD/Parsec/ToDesk/UU 等。
- 提供“打开 Windows 显示设置”“安装 VDD（外部链接）”“推荐假负载”的引导。
- 允许用户自行安装第三方虚拟显示器，项目只做兼容和提示；不自带、不静默装。
- 如果用户已有 ToDesk/UU/向日葵，可直接利用它们的虚拟屏/超级屏，不必重复造轮子。

**长期（只有产品必须“单安装包、开箱即用无头”时才做）**

- 自研 IDD 或与 VDD 合作，解决签名、分发许可、安装/卸载、GPU 驱动冲突、Windows 版本兼容。
- 需要 EV 证书/Partner Center/attestation 或 WHQL 的预算与时间，以及长期驱动维护。
- 需要接受“驱动问题会变成产品问题”：黑屏、显示优先级、更新回滚、企业拦截，都要有支持方案。
- 如果只是自用/少量机器，**假负载的性价比远高于自带驱动**。

---

## 4. 硬件方案：HDMI/DP 假负载（dummy plug）

### 4.1 原理

假负载（headless ghost / display emulator / EDID emulator）是一个小接头，内部通常是一颗预烧录 EDID 的 I²C EEPROM：

- 通过 HDMI/DP 接口取电，向 GPU 提供 EDID；
- 拉高 HPD（热插拔检测），让 GPU 认为“接了一块显示器”；
- GPU 按 EDID 初始化扫描输出和模式列表，Windows 就有活动桌面；
- 不需要驱动，BIOS/启动阶段就生效。

⚠️ 以上原理来自硬件厂商/社区技术说明；具体引脚/供电以产品为准。

### 4.2 价格量级

- 2026-11 检索：海外电商单只 HDMI 4K dummy 约 **5 美元上下**，多只装折合每只约 **1–3 美元**；主动式 DP 假负载也在类似量级，4K/高刷/HDR 型号更贵。
- 国内电商常见 **几十元人民币**一只；4K/高刷/主动式会更高。
- 价格波动大，以上只作量级参考，不构成采购建议。参考：[HDMI dummy plug 价格汇总](https://flapen.com/research/us/electronics/hdmi-dummy-plug)、[主动式 DP dummy 示例](https://www.newegg.com/p/30U-00PJ-000A0)。

### 4.3 优点

- 零驱动、零管理员权限、零签名问题。
- 在 BIOS/启动阶段就有效，适合无头开机、远程装机、BitLocker/登录界面之前的场景。
- 便宜、即插即用、坏了换一个。
- 对 GDI/DXGI/WGC 都有效，因为系统真的认为有显示器。
- 不依赖 Windows 版本/驱动版本。

### 4.4 缺点

- EDID 固定：分辨率/刷新率/色深受假负载限制，不能像 IDD 那样随意改。
- 低端被动式通常只广告 1080p60 或 4K30/4K60；HDR/VRR/DSC/高刷可能没有或会降级。
- 占用一个视频输出口；机器需要物理接触。
- DP 假负载有主动/被动差异，部分被动式在 DP 上不稳定；4K/高刷建议选主动式。
- 不能解决“多虚拟屏/自定义分辨率/超级屏”的软件需求，只是让系统有显示输出。
- 小接头可能松动/丢失；机房需要固定。

### 4.5 什么时候最省事

- 机器在机房/家里角落，没人会去改分辨率；
- 只需要 1080p 或 4K60 的稳定桌面；
- 希望开机即用、不装驱动、不给用户管理员权限；
- 远程装机、BIOS 设置、登录界面也要看到；
- 作为 IDD 的兜底：驱动出问题时，假负载仍能保证有画面。

**建议**：无头机器优先买一个与目标分辨率匹配的 HDMI 假负载（DP 机器选主动式 DP）；如果是 4K/高刷/HDR，先确认假负载的 EDID 能力，不要买最便宜的被动式。

---

## 5. 超级屏与多屏拼接

### 5.1 “超级屏”到底指什么

没有一个跨厂商统一的“超级屏”标准。至少有两种不同东西被混叫：

1. **多屏拼接/合屏**：把多块物理显示器的画面按虚拟桌面排列拼成一张大画面，远端看到一个窗口。
2. **虚拟显示器/虚拟屏**：创建一块系统里不存在的显示器（IDD），可以自定义分辨率/刷新率，甚至把多块屏的内容聚合到这块虚拟屏上。

不同厂商的叫法和实现：

| 厂商/功能 | 官方描述 | 实质 | 能否无头 | 备注 |
|---|---|---|---|---|
| **网易 UU 远程“超级屏”** | 官方帮助：“超级屏本质就是虚拟屏”；突破物理屏幕分辨率/帧率限制，最高 4K/144；把被控端所有物理/虚拟屏统一合并为一个超级屏窗口；使用期间物理屏关闭；驱动异常错误码 301 提示需 Win10+ | **虚拟显示器 + 多屏聚合** | 可以（有虚拟输出） | 官方明确“虚拟屏”，且会关闭物理屏；属于 VIP 权益（媒体口径，具体以官网为准） |
| **ToDesk“虚拟屏”** | 官方帮助：为无显示器/服务器解决画面异常/分辨率小；最高 5120×1440，最多 3 个虚拟屏；Win10 1903+；可配合关闭物理显示器（仅性能版） | **虚拟显示器（IDD 类）** | 可以 | 另有“全部平铺”的多屏查看方式；官方未在帮助页明说驱动类型，按 IDD 理解（❓ 推测） |
| **向日葵“合屏/分屏/单屏”** | 官方帮助：合屏把多块画面放进一个窗口，分屏各自独立窗口，单屏只看一块；要求 Windows 个人版 V13.2+ 和瓜子会员 | **客户端查看方式/多屏布局**，官方未说明创建虚拟显示器 | 合屏本身不解决无头 | 另有“虚拟屏”勾选项，用于关屏/无显示器黑屏；这是两个不同能力 |
| **Parsec VDD** | 官方支持：为 Windows 主机启用虚拟显示，最多 3 个；只在 Parsec 会话期间存在，不能持久 | **虚拟显示器** | 会话期间可以 | 面向 Teams/Warp；不是通用常驻虚拟屏 |
| **本项目当前“超级屏”** | `docs/m3.md`：采集整块 Windows 虚拟桌面，多显示器按系统排列拼成一张画面 | **纯软件多屏拼接（虚拟桌面包围盒）** | 不可以 | 零依赖；OS 仍把每块屏当独立显示器 |

结论：

- **UU 的“超级屏”是虚拟显示器方案，不是单纯把多屏拼起来**；它能把所有内容聚合到一块虚拟屏，并关闭物理屏。
- **ToDesk 的“虚拟屏”也是虚拟显示器**；它的“多屏平铺”是查看方式。
- **向日葵的“合屏”是查看方式**；它的“虚拟屏”是另一个开关。
- **本项目的“超级屏”目前是纯软件拼接**：视觉上像一张大画面，但系统里仍是多块显示器。要变成 UU 那种“真超级屏”，需要 IDD。

### 5.2 纯软件拼接：能做什么，不能做什么

✅ 官方 API 事实：

- `SM_XVIRTUALSCREEN` / `SM_YVIRTUALSCREEN`：虚拟屏幕左上角坐标，副屏在主屏左侧/上方时**可以为负**。
- `SM_CXVIRTUALSCREEN` / `SM_CYVIRTUALSCREEN`：所有显示器包围盒的宽高。
- `SM_CMONITORS`：桌面上的显示器数量（只计可见显示器，和 `EnumDisplayMonitors` 不完全等价）。
- `GetDC(NULL)`：整个屏幕的 DC。
- `GetMonitorInfo` 的 `rcMonitor` 是虚拟屏幕坐标，非主屏可能为负。
- 参考：[GetSystemMetrics](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getsystemmetrics)、[MONITORINFO](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-monitorinfo)、[EnumDisplayMonitors](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-enumdisplaymonitors)、[GetDC](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getdc)。

本项目当前做法（`crates/agent/src/capture.rs`）：

- 取虚拟桌面 `src_x/src_y/src_w/src_h`；
- `StretchBlt` 的源矩形直接用这组坐标，目标按 `max_width` 等比缩放；
- 单显示器时虚拟桌面等于主屏，行为与只采主屏一致；
- 输出宽 `min(src_w, max_width)`，不放大；高按比例四舍五入。

能：

- 一次抓到所有活动显示器按系统排列拼成的整张画面；
- 支持横向、纵向、负原点（副屏在左/上）、旋转；
- 不需要驱动、管理员权限、签名；
- 与现有 GDI/JPEG 管线完全兼容；
- 鼠标绝对坐标用 `MOUSEEVENTF_VIRTUALDESK` 就能覆盖整块虚拟桌面。

不能：

- 不能创造显示器；没有活动显示器时仍然黑屏/失败；
- 不能提高物理分辨率，不能自定义刷新率；
- OS 和应用仍把每块屏当独立显示器：窗口最大化只占一块，应用不会把多屏当成一块超大画布；
- 非矩形排列时，包围盒里的空档会显示为黑色/桌面背景；
- 每块屏 DPI 不同时，画面上的文字大小不一致（这不是坐标错误，是各屏缩放不同）；
- 屏幕很多时，压到 `max_width=1280` 后文字会非常小；
- 显示器热插拔/改分辨率后，已经启动的采集线程不会自动换尺寸（当前实现缓存了源矩形和 DIB 尺寸），需要重新订阅或加自动重建。

### 5.3 多屏排列、坐标与缩放

**横向排列**：`src_w = 各屏宽度之和`，`src_h = 最大高度`；如果高度不同，包围盒底部会有空档。

**纵向排列**：`src_h = 各屏高度之和`，`src_w = 最大宽度`；右侧可能空档。

**混合/错位（L 形）**：包围盒包含未覆盖区域；GDI 会采到黑色/桌面背景。验收时要明确“黑块是正常空档，不是采集失败”。

**负原点**：副屏在主屏左边时 `SM_XVIRTUALSCREEN` 为负；`StretchBlt` 的源 x/y 必须用这个负值，不能从 0 开始，否则会裁掉/错位。社区常用 `GetDC(NULL)` + `SM_*VIRTUALSCREEN` 抓全屏，参考 [StackOverflow 示例](https://stackoverflow.com/a/28248531)。

**旋转**：竖屏的虚拟桌面尺寸已经是旋转后的；GDI 采到的是旋转后的画面。DXGI 的 surface 可能是未旋转的，需要看元数据；如果以后切 DXGI，不能直接假设 surface 方向。

**缩放与可读性**：

- 当前输出宽度上限默认 1280。3 台 4K 横排的包围盒可能是 11520×2160，缩到 1280×240，文字几乎不可读。
- 对屏幕墙缩略图可以接受；单台控制如果也走这条流，建议提高 `max_width`（当前上限 1920）或按屏拆流。
- 缩小用 `HALFTONE` 更平滑，但比 `COLORONCOLOR` 慢；多屏大包围盒 + 10fps 时要测 CPU。
- `CAPTUREBLT` 会包含分层窗口，但可能更慢；当前已有失败回退 `SRCCOPY`。

**内存**：当前实现直接 `StretchBlt` 到目标尺寸的 DIB，不分配整张原始桌面，所以内存只跟输出尺寸有关；代价是每次都要从大源矩形缩放，GDI 压力较大。

### 5.4 DPI 缩放：最大的坐标风险

✅ 官方高 DPI 文档（[High DPI Desktop Application Development on Windows](https://learn.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development-on-windows)）：

- DPI unaware 应用：Windows 会对应用做位图拉伸；`GetSystemMetrics` 等拿到的是虚拟化后的值，不是真实物理像素。
- Per-Monitor V2（PMv2）应用：**看到每个显示器的原始像素，Windows 不做位图拉伸**；DPI 变化通过 `WM_DPICHANGED` 通知。
- PMv2 从 Windows 10 1703 起支持。

对本项目的含义：

- 进程必须是 **Per-Monitor V2**，否则在 125%/150% 缩放下，`GetSystemMetrics(SM_*VIRTUALSCREEN)` 可能返回逻辑尺寸，而 `BitBlt` 拷的是物理像素，导致截图被裁切、偏移或缩放，鼠标映射也会错位。
- 当前代码里没有显式 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`；Tauri/Windows manifest 可能已经设置了，但**必须实机确认**。验收方法：在 150% 缩放的屏幕上比较 `GetSystemMetrics(SM_CXSCREEN)`、`GetMonitorInfo.rcMonitor` 和实际分辨率；如果数值是缩放后的，就说明进程不是 PMv2。
- `GetMonitorInfo.rcMonitor` 是虚拟屏幕坐标，非主屏可以为负；在 PMv2 下按物理像素理解。
- 混合 DPI 下，**如果按原始包围盒抓取**，图像坐标到虚拟桌面坐标是线性的（同一缩放因子），鼠标映射可以正确；但各屏 UI 大小不同，文字看起来一大一小。
- **如果自己把各屏缩放/对齐到统一画布**（例如把 4K 屏缩到 1080p 高度再拼接），映射就变成分段函数：每块屏有自己的偏移和缩放，必须按显示器矩形分别换算，不能再用一个全局线性映射。
- 远程画面在浏览器里用 `object-fit: contain` 时，前端要先扣掉黑边再归一化；本项目 M3 已经这样做（见 `docs/m3.md` 5.2 的坐标说明）。后端再把 0..1 映射到虚拟桌面。

**风险总结**：

- DPI unaware → 截图裁切/缩放 + 鼠标偏移，混合 DPI 必现；
- PMv2 + 原始包围盒 → 坐标线性、鼠标正确，但文字大小不一致、空档黑块；
- 自定义画布 → 画面更整齐，但映射复杂，必须逐屏换算；
- 会话中改缩放/分辨率 → 当前采集线程不会自动重建，必须重连或加 `WM_DISPLAYCHANGE` / 轮询重建。

### 5.5 鼠标交互：必须用 `MOUSEEVENTF_VIRTUALDESK`

✅ 官方 `MOUSEINPUT` 文档（[MOUSEINPUT](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-mouseinput)）：

- `MOUSEEVENTF_ABSOLUTE`：`dx/dy` 是 0..65535 的归一化坐标；**多显示器下默认只映射到主显示器**。
- `MOUSEEVENTF_VIRTUALDESK`：必须和 `MOUSEEVENTF_ABSOLUTE` 一起用；坐标映射到**整个虚拟桌面**。

因此：

- 只用 `ABSOLUTE` 不加 `VIRTUALDESK`，只能点主屏；副屏点不到。这是多屏远控最常见的 bug。
- 本项目 `input.rs` 已经使用 `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`，虚拟桌面指标无效时退回主屏；这是正确的。
- 画面是虚拟桌面包围盒时，前端归一化坐标可直接作为 `dx/dy`；`SendInput` 会映射到整个虚拟桌面。
- 包围盒里的空档没有显示器；点击空档可能被系统夹到最近的屏，产品上应避免在空档区域发送点击，或在 UI 上标出空档。
- 如果画面做了自定义缩放/拼接，不能直接把画面 0..1 交给 `VIRTUALDESK`；要先按每块屏的矩形反算虚拟桌面坐标，再归一化。
- `VIRTUALDESK` 的 0..65535 量化在高分辨率多屏下仍有亚像素误差，但通常小于 1 像素；超宽多屏（例如 11520 像素）步进约 0.18 像素，可接受。

### 5.6 拼接与虚拟显示器的组合

两者不是互斥的：

- 没有物理显示器时，先装 IDD/插假负载，让 Windows 有一块活动输出；
- 这块虚拟显示器会加入虚拟桌面；如果它和物理屏扩展排列，GDI 包围盒会自动把它拼进去；
- 只留虚拟显示器时，虚拟桌面就是虚拟屏，GDI 照常采集；
- 虚拟屏的分辨率/刷新率由驱动决定，可以设成 4K/高刷；采集端仍受 `max_width` 和 JPEG 码率限制。

产品上要区分两个能力：

1. **多屏拼接视图**：零依赖，解决“有多块屏，想在一个窗口里看全”；
2. **虚拟显示器**：解决“没有屏/想改变分辨率/想让系统多一块屏”，需要驱动。

### 5.7 建议路线

- **先做纯软件拼接（已经做了）**：零依赖、覆盖大多数“多屏查看/控制”需求；把产品文案写成“多屏拼接/完整桌面”，不要暗示它能创造显示器。
- **无头场景再引导**：检测到没有活动显示器/长时间黑屏时，推荐假负载或用户自行安装虚拟显示器；不要静默装驱动。
- **真“超级屏”作为后续能力**：如果确需 UU 那种“所有内容聚合到一块 4K/144 虚拟屏、物理屏关闭”，再上 IDD；这是另一个产品复杂度和支持成本层级。
- **不要自己改用户显示排列**：拼接只是采集/显示层的映射，不要为了“好看”去改 Windows 的显示器排列，否则用户本地布局会被破坏。

### 5.8 拼接验收

前置：两台显示器都接到被控端，Windows 设置为“扩展”，不是“复制”。

1. **横向排列**：1920×1080 + 1920×1080 左右排列。远端应看到 3840×1080 的整张桌面（按 `max_width` 缩放），两块屏内容都在；把窗口拖到副屏，远端能看到；点击副屏右下角，光标应落到副屏。
2. **纵向排列**：上下排列。远端应看到 1920×2160；检查上下内容和鼠标。
3. **负原点**：把副屏拖到主屏左边。`SM_XVIRTUALSCREEN` 应为负；远端画面不能左边被裁掉；点击最左侧应落到副屏。
4. **混合错位（L 形）**：副屏放在主屏右下方。远端包围盒会出现黑色空档；确认这是预期，不是采集失败。
5. **混合 DPI**：4K 屏设 150%，1080p 屏设 100%。确认截图是物理像素、文字大小不同但鼠标准确；如果截图被裁切/缩放，检查进程是否 PMv2。
6. **竖屏**：把一块屏旋转 90°。确认画面方向和鼠标位置一致。
7. **热插拔/改分辨率**：会话中拔掉副屏或改分辨率。当前实现不会自动重建，预期需要退出再进入控制页；如果加了自动重建，确认尺寸/鼠标都更新。
8. **三屏**：尽量用三块屏测试包围盒缩放后的可读性和 CPU/码率；如果文字太小，考虑提高 `max_width` 或按屏拆流。
9. **输入回归**：运行 `scripts/verify-m3.js`，确认 50%/50%、75%/50% 的光标落点；再手动点副屏四角。（注意脚本的坐标期望按主屏计算，做多屏测试时要先把控制页切回主显示器；脚本第 5 项断言的是「舞台内没有本地光标叠加层」，即不会出现两层光标，不是画面里能看到光标——光标由被控端合成在视频帧里。）
10. **记录**：记录显示器数量、`SM_*VIRTUALSCREEN` 四个值、每块屏 `rcMonitor`、DPI、输出分辨率、FPS/码率；这些是排查坐标问题的关键。

---

## 6. 检测与降级：怎么判断“没有活动显示器/采到黑屏”

### 6.1 可用信号

| 信号/API | 能说明什么 | 局限 |
|---|---|---|
| `GetSystemMetrics(SM_CMONITORS)` | 桌面上的可见显示器数量 | 只计可见显示器；无头时可能为 0，也可能有回退输出；不告诉你哪块是虚拟屏 |
| `SM_CXVIRTUALSCREEN` / `SM_CYVIRTUALSCREEN` | 虚拟桌面包围盒 | 为 0/负数说明没有有效桌面；正常也可能包含空档 |
| `EnumDisplayMonitors` + `GetMonitorInfo` | 枚举显示器矩形、主屏标志 | 可能包含镜像驱动的伪显示器；`rcMonitor` 可为负；DPI unaware 时可能虚拟化 |
| `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)` | 当前活动显示路径 | 无头时可能 0 路径；`GetDisplayConfigBufferSizes` 返回 0 时不要再调用 `QueryDisplayConfig`，否则可能 `ERROR_INVALID_PARAMETER`（见社区修复 [foundation-sunshine PR #601](https://github.com/AlkaidLab/foundation-sunshine/pull/601)） |
| `DisplayConfigGetDeviceInfo(GET_TARGET_NAME)` | 显示器友好名、EDID 厂商/产品号、`outputTechnology` | 需要遍历活动路径；虚拟屏名称不统一 |
| `DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY` | `16 = INDIRECT_WIRED`、`17 = INDIRECT_VIRTUAL` 通常表示间接/虚拟显示 | 不是所有虚拟屏都一定用这两个值；仍要结合名称 |
| `EnumDisplayDevices` + `DISPLAY_DEVICE_ATTACHED_TO_DESKTOP` | 设备是否附加到桌面 | 不等于“当前活动路径”；只作辅助 |
| `GUID_CONSOLE_DISPLAY_STATE` + `RegisterPowerSettingNotification` | OS 层显示器开/关/变暗 | 物理电源键不一定通知；Win8+；需要窗口/消息循环 |
| `WM_DISPLAYCHANGE` | 分辨率/显示拓扑变化 | 需要窗口过程；Tauri 进程要接消息或轮询 |
| `GetSystemMetrics(SM_REMOTESESSION)` | 当前是否 RDP 会话 | 不代表控制台会话状态；RDP 不是无头方案 |
| `StretchBlt` 返回值 + `GetLastError` | 采集调用本身是否失败 | 成功也可能全黑（DRM/黑桌面/空桌面） |
| 帧亮度统计 | 画面是否全黑/近乎全黑 | 真正黑屏内容、锁屏、DRM 也会触发；必须防抖和组合判断 |
| 分辨率是否 1024×768/800×600 | 可能是 no-EDID 回退 | 也可能用户真的用这个分辨率；不能单独判定 |

✅ `QueryDisplayConfig` 的 `QDC_ONLY_ACTIVE_PATHS`、`ERROR_ACCESS_DENIED`（远程会话）等语义见官方文档：[QueryDisplayConfig](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-querydisplayconfig)、[DisplayConfigGetDeviceInfo](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-displayconfiggetdeviceinfo)、[DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ne-wingdi-displayconfig_video_output_technology)、[DISPLAYCONFIG_TARGET_DEVICE_NAME](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_target_device_name)。

✅ `GUID_CONSOLE_DISPLAY_STATE` 的 off/on/dim 值和注册方式见 [Power Setting GUIDs](https://learn.microsoft.com/en-us/windows/win32/power/power-setting-guids)、[RegisterPowerSettingNotification](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerpowersettingnotification)。

### 6.2 “无活动显示器”判定

建议顺序：

1. **先看 `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)` 是否有路径**。0 条路径 = 没有活动显示输出，基本可以判定无头/全拔。
2. 再看 `SM_CXVIRTUALSCREEN` / `SM_CYVIRTUALSCREEN` 是否 > 0。<=0 说明没有可用桌面。
3. 再看 `SM_CMONITORS` / `EnumDisplayMonitors` 数量；为 0 或只有伪显示器时，提示无头。
4. 如果还有路径，但 `GetMonitorInfo` 的矩形异常（0 宽/0 高、巨大负值），也视为异常。
5. 最后才看黑帧；黑帧不能单独证明无头。

### 6.3 黑帧检测算法

在当前采集路径里，`StretchBlt` 成功后已经拿到了 BGRA DIB，可以在转 RGB/编码前抽样：

```text
对输出 DIB 按固定步长抽样（例如 64×36 个点）：
  luma = 0.2126*R + 0.7152*G + 0.0722*B
统计：
  mean、stddev、min、max、near_black_ratio（luma < 8 的比例）
判定（示例阈值，需按实际调）：
  if mean < 2 and stddev < 1 and near_black_ratio > 0.995:
      连续 N 帧（例如 5 帧）→ 标记“可能黑屏”
  else:
      清除计数
```

要点：

- 只在 `StretchBlt` 返回成功时做统计；失败单独报错。
- 不要因为 1 帧黑就断开/重连；用 3–5 帧防抖。
- 真正的黑屏内容（全屏视频、游戏、黑色壁纸）、锁屏、DRM 也会触发；提示要用“可能”，不要断言“显示器关了”。
- 分辨率 1024×768 只能作为辅助信号；不要因此自动改分辨率。
- 统计成本很低：64×36 = 2304 个采样点，相对于 JPEG 编码可以忽略。
- 如果输出尺寸很小（例如 1280×240），采样网格按实际尺寸缩放，避免越界。

### 6.4 建议的状态机

```text
每帧：
  ok = StretchBlt(...)
  if not ok:
      state = CAPTURE_ERROR
  else:
      stats = sample_luma(...)
      paths = QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)
      if paths == 0 or virtual_screen_invalid:
          state = NO_DISPLAY
      else if stats.black_for(N):
          state = MAYBE_BLACK
      else:
          state = OK

状态变化时：
  上报 status（显示器数量、虚拟桌面尺寸、活动路径数、是否虚拟屏、
            最后错误、黑帧统计、建议）
  不要自动安装驱动/改电源计划/改显示排列
  NO_DISPLAY / MAYBE_BLACK 时保留会话，继续低频重试
  检测到 WM_DISPLAYCHANGE / 路径变化 → 重建 Capturer（DIB、尺寸）
```

### 6.5 检测到之后怎么降级

- **客户端显示提示**：不要只给黑屏，给出“被控端可能没有活动显示器/显示器被关闭/正在使用虚拟屏”的原因和下一步。
- **保留最后一帧或占位图**：不要伪造桌面内容；可以显示“等待画面”状态。
- **继续保活**：不要因为黑屏就退出采集线程；保留重试，显示器恢复后自动继续。
- **自动重建**：显示器热插拔/分辨率变化时，重建 DIB 和源矩形；当前实现需要重连，建议后续加。
- **日志**：记录 `SM_*VIRTUALSCREEN`、每块屏 `rcMonitor`、`QueryDisplayConfig` 路径数、`outputTechnology`、友好名、`StretchBlt` 错误、黑帧统计。没有这些日志，远程排查无头问题几乎不可能。
- **用户指引**：
  - 先检查显示器电源/线缆/输入源；
  - 关屏但机器运行：可尝试移动鼠标/按键唤醒，或调整电源计划；
  - 无头：插 HDMI/DP 假负载，或安装虚拟显示器；
  - 笔记本：检查合盖动作和电源计划；
  - 企业机器：检查组策略/安全软件是否阻止驱动或采集。

### 6.6 虚拟显示器识别

用 `QueryDisplayConfig` 遍历活动路径，对每个 target 调 `DisplayConfigGetDeviceInfo(DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME)`：

- `outputTechnology == 16 (INDIRECT_WIRED)` 或 `17 (INDIRECT_VIRTUAL)`：很可能是间接/虚拟显示；
- `monitorFriendlyDeviceName` / `monitorDevicePath` 里可能包含 “Virtual Display”、“Parsec”、“USB Mobile Monitor”、“ToDesk”、“UU” 等字样；
- `edidManufactureId` / `edidProductCodeId` 只有 `edidIdsValid` 时有效；虚拟屏可能没有真实 EDID；
- 不要只靠名称硬编码；以 `outputTechnology` 为主，名称为辅；
- 识别到虚拟屏后，可以在 UI 里显示“当前使用虚拟显示器”，并提示分辨率/刷新率由该驱动决定。

---

## 7. 推荐路线

| 阶段 | 做什么 | 成本 | 风险/备注 |
|---|---|---|---|
| **P0 现在** | 保持虚拟桌面拼接 + `SetThreadExecutionState`；补检测/状态上报/黑帧防抖；显示器变化时自动重建；用户文档写清“关屏/无头”边界 | 低，零依赖 | 不能解决无头；热插拔/DPI 要验收 |
| **P1 无头兜底** | 推荐 HDMI/DP 假负载；在 UI/文档里给出型号能力要求（1080p60/4K60/主动 DP）；无头机器配自动登录 + 自启 | 低，几十元硬件 | 需要物理接触；EDID 固定 |
| **P2 虚拟屏引导** | 检测虚拟显示器；引导用户自行安装 VDD/ToDesk/UU/Parsec；不静默装；只做兼容和提示 | 中 | 第三方驱动版本/冲突/许可；不能保证用户体验 |
| **P3 自带驱动** | 自研 IDD 或与 VDD 合作；解决签名、安装/卸载、GPU 驱动冲突、Windows 更新兼容 | 高 | EV/Partner Center/WHQL、管理员、长期维护；只有“单安装包无头”才值得 |
| **P4 真超级屏** | 在 IDD 基础上做 4K/高刷虚拟画布 + 多屏聚合 + 物理屏关闭 | 高 | 这是 UU/ToDesk 级别产品能力，不是采集层小改 |

判断标准：

- 用户只是“有多块屏，想在一个窗口看全/操作” → **P0 纯软件拼接足够**。
- 用户是“机房无头服务器，开机就要能看/控” → **假负载最省事；需要自定义分辨率再上 IDD**。
- 用户要求“像 UU 超级屏一样，把所有内容放到一块 4K/144 虚拟屏，物理屏关闭” → **必须 IDD，纯软件做不到**。
- 产品要求“一个安装包，不额外买硬件，所有机器开箱即用” → **自带签名 IDD**，但要接受成本和维护。

---

## 7.5 本项目的落地：无头时自动接入 usbmmidd 虚拟屏

2026-11-25 实现，对应 `crates/agent/src/virtual_display.rs`。做法与 CrossDesk 一致：
随安装包带 Amyuni **usbmmidd_v2**（微软签名的间接显示驱动），检测到本机没有任何活动
显示输出时，装驱动并插一块 1920×1080 虚拟屏，再照常走 DXGI 采集。

- **开关**：设置 → 安全 → 被控授权 → 「无头时接入虚拟显示器」，默认关闭，切换即时生效。
  第一次真正需要时会在会话开始时弹 UAC；用户拒绝就继续黑帧，不阻塞会话。
- **判断**：`EnumDisplayDevicesW` 枚举活动适配器，数量为 0（且虚拟桌面尺寸为 0）才算
  headless。显示器只是断电、EDID 还在的情况**不算**，那种情况本来就能采到画面。
- **安装**：提权重跑自身（`--screenwall-vdisplay=plug`），在管理员进程里执行
  `deviceinstaller64.exe install usbmmidd.inf usbmmidd`、`enableidd 1`，成功后写
  `HKLM\Software\ScreenWall\UsbmmiddDriverInstalled = 1`（64 位视图）。
- **复用自己的屏**：桌面上已经有 usbmmidd 输出时不再插第二块。
- **已知限制**：插上的虚拟屏不会在物理显示器接回后自动拔掉，会一直留到卸载；接回真屏后
  可以在 Windows 显示设置里禁用这块 1920×1080。UAC 被取消后 60 秒内不再重复弹窗。
- **卸载**：NSIS 的 `PREUNINSTALL` 先 `enableidd 0` 逐块拔屏，再 `stop` / `remove`；
  只有上面的注册表标记存在才删，用户自己为别的软件装的同名驱动不动。
- **第三方边界**：驱动包原样放在 `third_party/usbmmidd_v2/`，声明与许可见
  `third_party/usbmmidd_v2/NOTICE.md`。许可要求保留版权声明，且首次使用可能弹一次
  广告页；`usbmmIdd.inf` 不能改，改了签名就失效。
- **排查**：手动指定驱动目录用环境变量 `SCREENWALL_USBMMIDD_DIR`；日志里搜
  「无头虚拟显示器」。开发机没接显示器时可以用它验证，但**不要**在无人值守机器上
  自动点 UAC。

## 8. 验收方法

以下步骤用于在真实机器上确认“关显示器、拔线、改电源计划、装虚拟屏”分别会怎样。建议用两台机器：被控端 + 主控端；记录 GPU、接口（HDMI/DP）、显示器型号、Windows 版本、驱动版本。

### 8.1 基线

1. 被控端接一台显示器，设置为 1920×1080（或原生分辨率）。
2. 启动服务器、客户端，登录同一账号，打开单台控制页。
3. 确认实时画面、FPS、分辨率正常。
4. 记录 `GetSystemMetrics` 的 `SM_CXSCREEN/SM_CYSCREEN`、`SM_*VIRTUALSCREEN`、`SM_CMONITORS`（可在日志/临时诊断工具里打印）。
5. 运行 `scripts/verify-m3.js`，确认键鼠坐标基线（期望落点按主屏计算，先切回主显示器；第 5 项检查的是「没有本地光标叠加层」，见 5.8 第 9 步的说明）。

### 8.2 电源计划关屏 / DPMS

1. 保持控制页在线。
2. 运行 `scripts/test-monitor-off.ps1`（当前脚本只抓主屏，用于单屏验证）：
   - 记录 baseline 的 mean/stddev；
   - 广播 `SC_MONITORPOWER` 关屏；
   - 等 3 秒，重新抓屏；
   - 记录 while off 的 mean/stddev 和 pixel diff；
   - 恢复显示器。
3. 预期：`RESULT: PASS`，远端画面仍有内容；本地显示器黑。
4. 再测 Windows 电源计划：
   ```powershell
   powercfg /change monitor-timeout-ac 1
   powercfg /change standby-timeout-ac 5
   ```
   保持采集线程运行，等待超过 1 分钟；显示器应不被系统关闭（`ES_DISPLAY_REQUIRED`）。用 `powercfg /requests` 查看是否有 DISPLAY/SYSTEM 请求。
5. 停止采集（退出控制页/停止 agent），等待；确认电源计划恢复生效，`powercfg /requests` 清空。
6. 恢复电源计划：
   ```powershell
   powercfg /change monitor-timeout-ac 10
   powercfg /change standby-timeout-ac 30
   ```
   （按用户原设置恢复，不要留在“从不”。）

### 8.3 显示器电源键 / 拔线

分别对 HDMI 和 DP 做：

1. 连接正常，控制页有画面。
2. **按显示器电源键关闭**（不是 Windows 关屏），等待 10 秒。
3. 观察：
   - 本地显示器是否黑；
   - Windows 是否播放设备断开提示音/显示设置里少了一块；
   - 远端画面是否还在、是否黑、是否只剩其他屏。
4. 重新打开显示器，等待；观察是否恢复；当前采集线程可能不会自动重建，必要时退出再进入控制页。
5. **直接拔掉视频线**，等待 10 秒，重复观察；再插回。
6. 对 DP 特别注意：断电/拔线后 HPD 可能掉，Windows 会重排桌面；这是“拔线”行为，不是“关屏”行为。
7. 记录：接口、显示器是否保持 EDID/HPD、Windows 是否重排、远端是否黑、恢复是否需要重连。

### 8.4 无头开机

1. 关机，拔掉所有显示器。
2. 开机，等待自动登录/托盘进程启动（如果没配自动登录，先配好）。
3. 从另一台机器连接：
   - 有画面？分辨率多少？是否 1024×768/800×600？
   - 黑屏？检查 agent 日志里的显示器数量/虚拟桌面尺寸/`QueryDisplayConfig` 路径数。
4. 插入 HDMI 假负载（或主动 DP 假负载），重启；重复连接，预期有稳定画面和 EDID 分辨率。
5. 或安装 VDD/其他虚拟显示器，重启；确认 Windows 显示设置里出现虚拟显示器，远端有画面。
6. 记录：无硬件/假负载/IDD 三种情况下的行为；这是产品文档必须写清的部分。

### 8.5 笔记本合盖

1. 设置合盖动作（控制面板/设置 → 电源 → 合盖时）：
   - “不采取任何操作”；
   - “睡眠”。
2. 分别测试：
   - 只关内屏（如果有外接屏）：远端是否还能看到外接屏/内屏；
   - 合盖睡眠：远端是否断连/黑屏；
   - 合盖“不操作”但固件强制关内屏：远端是否黑。
3. 记录：合盖动作、是否有外接屏、是否睡眠、远端行为。
4. 结论：笔记本不适合作为严格无人值守的无头服务器；如果必须用，配假负载/IDD，并确认固件行为。

### 8.6 虚拟显示器安装/卸载

以 VDD 为例（具体以项目 README 为准）：

1. 从 [Virtual Display Driver releases](https://github.com/VirtualDrivers/Virtual-Display-Driver/releases) 下载 VDC/安装器；确认来源是 `VirtualDrivers` 组织。
2. 运行 VDC，安装驱动；按提示可能需要 VC++ 运行库/重启。
3. 打开 Windows 显示设置，确认出现虚拟显示器；设置分辨率/刷新率/扩展排列。
4. 远端连接，确认画面包含虚拟显示器；在虚拟屏上打开窗口，确认能采到。
5. 用 `QueryDisplayConfig`/设备管理器确认 `outputTechnology` 和友好名。
6. 卸载 VDD，重启；确认虚拟显示器消失、显示设置恢复、远端仍能采物理屏。
7. 记录：安装是否需要重启、是否触发 UAC/安全软件、是否有黑屏/显示优先级问题、卸载是否干净。

usbmmidd 的验收类似，但注意官方只写 Win10、默认 1920×1080、最多 4 个、安装需管理员、可能有广告页。

ToDesk/UU/向日葵：如果用户有授权，直接测试它们的“虚拟屏/超级屏/合屏”：
- 开启后物理屏是否关闭；
- 远端是否仍能看到；
- 分辨率/刷新率是否符合官方描述；
- 退出后是否恢复；
- 是否需要会员/特定版本。

### 8.7 多屏拼接

按 5.8 的 10 项做。重点记录：

- `SM_XVIRTUALSCREEN/Y/CX/CY`；
- 每块屏的 `rcMonitor`、DPI；
- 输出分辨率、FPS、码率；
- 负原点、L 形空档、竖屏、热插拔；
- 鼠标是否能到副屏四角；
- 混合 DPI 下截图是否裁切/缩放。

### 8.8 RDP / 锁屏 / 会话

1. 锁屏（Win+L），远端看画面；再解锁。
2. 注销当前用户，远端看画面；再登录。
3. 用 RDP 连接被控端，再从另一个客户端看 ScreenWall 画面；断开 RDP，观察。
4. 预期：锁屏/注销/RDP 断开可能导致黑屏/`SESSION_DISCONNECTED`；这不是虚拟显示能完全解决的，登录界面/安全桌面不在 v1 范围。
5. 记录实际行为，写进用户文档，避免用户误以为“无头 = 能看登录界面”。

### 8.9 验收记录表

| 场景 | 预期 | 实际 | 通过 | 备注 |
|---|---|---|---|---|
| DPMS 关屏（单屏） | 远端仍有画面 |  |  | 本机已 PASS |
| 电源计划 1 分钟关屏 + 保活 | 不关屏、不睡眠 |  |  | `powercfg /requests` |
| 显示器电源键关（HDMI） | 待实测 |  |  | 是否掉 HPD |
| 显示器电源键关（DP） | 待实测 |  |  | 是否重排桌面 |
| 拔线 | 剩余屏可采/无屏黑 |  |  | 恢复是否需重连 |
| 无头开机 | 黑/1024×768/无输出 |  |  | 记录具体 |
| 假负载 + 无头 | 稳定分辨率 |  |  | EDID 能力 |
| VDD + 无头 | 稳定分辨率/刷新率 |  |  | 安装/卸载 |
| 双屏横向 | 整张桌面、鼠标可达副屏 |  |  | 5.8 |
| 负原点/竖屏/L 形 | 不裁切、空档黑 |  |  | 记录 |
| 混合 DPI | 物理像素、鼠标准确 |  |  | PMv2 |
| 热插拔/改分辨率 | 需重连或自动重建 |  |  | 当前限制 |
| 锁屏/注销/RDP | 黑/断会话 |  |  | 文档说明 |

---

## 9. 事实、推测与未决问题

### 9.1 已验证事实（官方文档/可复现）

- `GetDC(NULL)` 返回整个屏幕 DC；GDI 抓的是桌面合成表面，不是显示器信号。
- 虚拟屏幕指标 `SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN/SM_CXVIRTUALSCREEN/SM_CYVIRTUALSCREEN` 定义所有显示器的包围盒；非主屏坐标可为负。
- `GetMonitorInfo.rcMonitor` 使用虚拟屏幕坐标，非主屏可为负。
- `MOUSEEVENTF_ABSOLUTE` 默认只映射主屏；加 `MOUSEEVENTF_VIRTUALDESK` 才映射整个虚拟桌面。
- DXGI Desktop Duplication 按活动输出复制；复制整个桌面要每输出一路再合并；`WAIT_TIMEOUT`、`ACCESS_LOST`、`SESSION_DISCONNECTED` 的语义如官方文档。
- `SetThreadExecutionState` 的 `ES_DISPLAY_REQUIRED` / `ES_SYSTEM_REQUIRED` / `ES_CONTINUOUS` 语义；不能阻止用户主动睡眠，不能停止屏保。
- IDD 是 UMDF 用户态驱动，运行在 session 0；创建虚拟显示器并报告显示模式；最低 Win10 1709；IddCx 1.4 对应 1903。
- 内核模式驱动从 Win10 1607 起需要 Dev Portal 签名；Dashboard 需要 EV 证书；attestation/WHQL 的定位如官方文档；测试签名不适合正式分发。
- UMDF 驱动不需要内核 Code Integrity 签名，但驱动包仍需签名（官方旧文）。
- Tauri NSIS `perMachine` 需要管理员权限。
- `pnputil /add-driver` / `/delete-driver` 的用法。
- 本机 DPMS 关屏 + GDI 抓屏 PASS（单机、单屏、1920×1080）。
- UU“超级屏”官方定义是虚拟显示方案；ToDesk 虚拟屏官方要求 Win10 1903+、最高 5120×1440、最多 3 个；向日葵合屏官方说明；Parsec VDD 官方说明；VDD/usbmmidd/IddSampleDriver 的许可证和安装方式如各自仓库/文档。

### 9.2 社区/实测共识（非官方保证）

- 显示器物理断电/拔线/无头时，GDI 常黑屏或失败；DP 比 HDMI 更容易掉 HPD。
- 无头 Windows 可能回退到 1024×768/800×600，也可能没有活动输出。
- DXGI 在显示器关闭/无更新时会大量 `WAIT_TIMEOUT`；某些驱动/场景会黑屏或 `ACCESS_LOST`。
- WGC 在无头/关屏时也可能黑；它不是无头方案。
- RealVNC 官方支持文档提到：无显示器或显示器关闭时，Windows 可能不能正确报告图形更新；建议虚拟显示驱动或调整采集方式（[RealVNC 支持文章](https://help.realvnc.com/hc/en-us/articles/360004012211-Display-issues-when-connecting-to-RealVNC-Connect-for-Windows)）。
- 向日葵官方帮助也明确：被控端显示器关闭/没有显示器时可能黑屏，解决方式是保持显示器开启、开启“虚拟屏”、或用 HDMI 诱骗器/虚拟显示器。
- VDD 项目自述 release 驱动已签名（SignPath），支持 Win10/11；usbmmidd 说明称驱动经 Microsoft 签名流程；Parsec VDD 支持 Win10 21H2+、最多 3 个、非持久。

### 9.3 推测（需在目标机器验证）

- “显示器电源键关闭但线接着”是否仍被 Windows 识别，取决于显示器是否保持 HPD/EDID；HDMI 通常更宽容，DP 更容易掉。
- DPMS 关屏下 DXGI 是否始终能复制/是否黑屏，取决于 GPU/驱动；本文只确认 GDI 本机可用。
- 无头开机是否一定回退 1024×768，取决于 GPU/驱动/BIOS；可能没有输出。
- IDD 虚拟显示器在 GDI 包围盒里一定能被采到：按“IDD 参与桌面合成”的机制推断，应该可以；需实测。
- 虚拟屏在重启后是否自动存在，取决于驱动/配置（VDD 可能常驻，ToDesk/UU 可能按会话创建）。
- UMDF IDD 的最低签名要求能否用普通 Authenticode/OSS 签名满足，取决于目标 Windows 策略和分发方式；正式分发仍建议 Microsoft 签名。
- VDD 已签名二进制能否随本项目再分发，需核实许可/签名条款。

### 9.4 未决问题

- 目标用户的机器是 HDMI 还是 DP？是否愿意插假负载？
- 产品是否必须“无头开机、开箱即用”，还是可以要求用户自行安装虚拟显示器？
- 是否需要“真超级屏”（4K/144、多屏聚合、物理屏关闭），还是“多屏拼接视图”就够？
- 是否愿意承担驱动签名/安装/卸载/GPU 更新冲突的长期维护？
- 是否支持 Windows 10 1903 以下？IDD/ToDesk/UU 都有最低版本要求。
- 是否需要采集登录界面/锁屏/UAC 安全桌面？v1 明确不做，那无头机器必须自动登录。

---

## 10. 参考资料

### Microsoft 官方

- [GetDC](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getdc)
- [GetSystemMetrics](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getsystemmetrics)
- [GetMonitorInfo](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getmonitorinfow)
- [MONITORINFO](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-monitorinfo)
- [EnumDisplayMonitors](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-enumdisplaymonitors)
- [QueryDisplayConfig](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-querydisplayconfig)
- [DisplayConfigGetDeviceInfo](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-displayconfiggetdeviceinfo)
- [DISPLAYCONFIG_TARGET_DEVICE_NAME](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_target_device_name)
- [DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ne-wingdi-displayconfig_video_output_technology)
- [MOUSEINPUT](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-mouseinput)
- [Desktop Duplication API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)
- [DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput)
- [AcquireNextFrame](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe)
- [SetThreadExecutionState](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setthreadexecutionstate)
- [Indirect display driver overview](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview)
- [IddCxGetVersion](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxgetversion)
- [Driver Signing Policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)
- [Driver code signing requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs)
- [Attestation sign Windows drivers](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/attestation-signing-a-kernel-driver-for-public-release)
- [Test Signing](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/test-signing)
- [Do UMDF drivers require signing?](https://learn.microsoft.com/en-us/archive/blogs/peterwie/do-umdf-drivers-require-signing)
- [PnPUtil 命令语法](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/pnputil-command-syntax)
- [Powercfg 命令行选项](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/powercfg-command-line-options)
- [Power Setting GUIDs](https://learn.microsoft.com/en-us/windows/win32/power/power-setting-guids)
- [RegisterPowerSettingNotification](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerpowersettingnotification)
- [High DPI Desktop Application Development on Windows](https://learn.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development-on-windows)

### 驱动/项目

- [Microsoft IddSampleDriver（Windows-driver-samples / IndirectDisplay）](https://github.com/microsoft/Windows-driver-samples/tree/main/video/IndirectDisplay)
- [Virtual Display Driver（MIT）](https://github.com/VirtualDrivers/Virtual-Display-Driver)
- [Virtual Display Driver releases](https://github.com/VirtualDrivers/Virtual-Display-Driver/releases)
- [Intel IDD readme（明确“未签名，需签名或测试模式”）](https://github.com/intel/cloud-streaming/blob/main/sources/drivers/idd/readme.rst)
- [usbmmidd v2 官方下载（Amyuni）](https://www.amyuni.com/downloads/usbmmidd_v2.zip)
- [Parsec VDD 官方支持文章](https://support.parsec.app/hc/en-us/articles/4422939339789)
- [Parsec App for Windows / 无头虚拟显示](https://support.parsec.app/hc/en-us/articles/32381199341716)

### 厂商/社区

- [网易 UU 远程：什么是超级屏](https://uuyc.163.com/help/superscreen.html)
- [ToDesk：远程怎么使用虚拟屏](https://todesk.com/helpcenter/enterprise-231.html)
- [ToDesk 多屏控制与虚拟屏介绍](https://www.todesk.com/special_multicontrolandvirtualscreen.html)
- [向日葵：远程控制电脑如何同时查看多个屏幕](https://service.oray.com/question/63381.html)
- [向日葵：远程桌面卡顿黑屏解决方案（含虚拟屏/诱骗器）](https://service.oray.com/question/54556.html)
- [RealVNC：Display issues when connecting to RealVNC Connect for Windows](https://help.realvnc.com/hc/en-us/articles/360004012211-Display-issues-when-connecting-to-RealVNC-Connect-for-Windows)
- [SuperUser：headless Windows 10 1024×768](https://superuser.com/q/1726706)
- [SuperUser：DisplayPort 显示器关闭导致断开](https://superuser.com/a/631853)
- [DataPro：Hot Plug Detection, DDC, and EDID](http://www.datapro.net/techinfo/hot_plug_detection.html)
- [StackOverflow：GetDC(NULL)/虚拟屏幕截图示例](https://stackoverflow.com/a/28248531)
- [foundation-sunshine PR #601：headless 时 QueryDisplayConfig 返回 0 路径的处理](https://github.com/AlkaidLab/foundation-sunshine/pull/601)
- [o3de-diorama：headless 录制/黑帧方差检测](https://github.com/nickschuetz/o3de-diorama/blob/main/Docs/howto/12-recording-demos.md)
- [HDMI dummy plug 价格汇总（2026-11 检索，仅量级参考）](https://flapen.com/research/us/electronics/hdmi-dummy-plug)
- [主动式 DisplayPort dummy 示例（Newegg）](https://www.newegg.com/p/30U-00PJ-000A0)
- [Tauri Windows Installer（perMachine 需要管理员）](https://v2.tauri.app/distribute/windows-installer/)

### 本项目内部

- `docs/m3.md`：虚拟桌面采集、`SetThreadExecutionState`、本机 DPMS 关屏实测
- `docs/design.md`：采集/输入设计、per-monitor v2 DPI 目标、非目标
- `crates/agent/src/capture.rs`：当前虚拟桌面采集和保活实现
- `crates/agent/src/input.rs`：`MOUSEEVENTF_VIRTUALDESK` 绝对坐标
- `crates/agent/src/display.rs`：`EnumDisplayMonitors` 显示器枚举
- `scripts/test-monitor-off.ps1`：DPMS 关屏验证脚本
