# 持续集成（CI）

本仓库的 CI 由两部分组成：

| 位置 | 用途 |
| --- | --- |
| `.github/workflows/ci.yml` | 推到 GitHub 后在 Actions 里自动跑 |
| `scripts/ci.ps1` | 本地跑**同样**的检查，不依赖 GitHub |

> **当前目录不是 git 仓库**（2026-11-24 实测 `git -C D:/screenwall status` 返回
> `fatal: not a git repository`）。因此 workflow 目前**不会**被触发。要用 CI 需要先：
>
> ```powershell
> cd D:\screenwall
> git init -b main
> git add .
> git commit -m "init"
> git remote add origin https://github.com/<you>/<repo>.git
> git push -u origin main
> ```
>
> 推送后 GitHub 会自动读取 `.github/workflows/ci.yml`。在此之前，用
> `scripts\ci.ps1` 在本地做等价检查。

---

## 1. 本地一键检查：`scripts\ci.ps1`

```powershell
cd D:\screenwall
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\ci.ps1
```

参数：

| 参数 | 作用 |
| --- | --- |
| `-SkipFmt` | 跳过 `cargo fmt --check` |
| `-SkipClippy` | 跳过 clippy |
| `-SkipTest` | 跳过 `cargo test` |
| `-SkipUi` | 跳过 `npm ci` / `tsc --noEmit` / `npm run build` |

依次执行：

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets`
3. `cargo test --workspace`
4. `ui`：`npm ci`（仅当 `ui\node_modules` 不存在时）→ `npm run typecheck`（即 `tsc --noEmit`）→ `npm run build`

每一步打印耗时与 `PASS`/`FAIL`，任一步失败脚本以退出码 1 结束，最后打印汇总表。

**注意**：脚本内部所有 cargo 调用都走 `scripts\cargo.bat`（`cmd /c`），因为 Git Bash
自带的 `/usr/bin/link.exe` 会遮蔽 MSVC 的链接器。直接 `cargo` 会在链接阶段报错。

---

## 2. GitHub Actions：`.github/workflows/ci.yml`

触发：`push` 到 `main` / `develop`，以及所有 `pull_request`。

| Job | 内容 | 阻塞？ | timeout |
| --- | --- | --- | --- |
| `fmt` | 对 `proto` / `server` / `agent` / `client` 逐个 `cargo fmt -p screenwall-<crate> -- --check` | 阻塞（2026-11-24 起已全绿） | 20 min |
| `build-and-test` | `cargo test --workspace` + `npm ci` + `npx tsc --noEmit` + `npm run build` | 阻塞 | 60 min |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` | 阻塞（2026-11-24 起零告警） | 45 min |
| `bundle` | `needs: build-and-test`，仅 `push` 到 `main`；装 NSIS → `scripts\tauri-build.bat build --bundles nsis` → 上传 `target/release/bundle/nsis/*.exe` → step summary 打印 SHA256 | 阻塞 | 90 min |

`fmt` 单独成 job 的原因：如果把它塞进 `build-and-test` 第一步，fmt 失败会让后面的
test / tsc / build **全被跳过**，看不到真实结果。拆开后即使 fmt 红，测试结果也照常产出。

缓存用 `Swatinem/rust-cache@v2`（`workspaces: "."`）；clippy job 额外加了 `key: clippy`
以免和 test job 互相污染缓存。

`bundle` job 里 NSIS 处理：`windows-latest` 镜像通常自带 NSIS，脚本仍会检查
`%ProgramFiles(x86)%\NSIS\makensis.exe`，没有就 `choco install nsis`，再检查一次，
确保不是「镜像哪天没带就静默跳过」。

---

## 3. 实测状态（2026-11-24，本机 Windows 11 + Rust 1.100.0）

所有结论均来自本机真实执行，CI 本身未在 GitHub 上跑过（目录不是 git 仓库）。

| 检查 | 结果 | 说明 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | **通过** | 0 处 diff |
| `cargo clippy --workspace --all-targets -- -D warnings` | **通过** | 0 条警告 |
| `cargo test --workspace` | **通过** | 214 passed / 0 failed |
| `npx tsc --noEmit` | **通过** | 无输出 |
| `npm run build` | **通过** | 1618 modules，约 1.8 s |
| `scripts\ci.ps1` | **通过** | 见下 |

### 3.1 rustfmt 已全量收口

本轮做了一次 `cargo fmt --all`，之后 `cargo fmt --all -- --check` 退出码 0、无 diff。
格式化是单独一步完成的（其余功能改动已完成），所以没有和并行改动冲突。

如果以后新增代码破坏了 fmt，`fmt` job 会立刻变红，本地用同一条命令修复即可：

```powershell
scripts\cargo.bat fmt --all
```

### 3.2 clippy 已清零（23 → 0）

原先的 23 条警告已全部修掉，`ci.yml` 里 clippy job 因此改成阻塞并加 `-- -D warnings`：

| crate | 原警告数 | 修法 |
| --- | --- | --- |
| `screenwall-agent` | 12 | `chunks_exact(_mut)` → `as_chunks(_mut)`；`map_or(true, …)` → `is_none_or`；`spawn` 返回类型提成 `SpawnedCapture` / `ReadySize` 别名；`bytes / frames` 改 `checked_div`；`if let Some(..)` 收进外层 `matches!` |
| `screenwall-client` | 6 | `map_or(true, …)` → `is_none_or`；`map_or(false, …)` → `is_some_and`；`unwrap_or(0)` 改 `map_or(0, …)`；多余的 `use anyhow::Context` 删掉 |
| `screenwall-server` | 5 | `Session::new` 的 8 个位置参数改成 `SessionInit` 结构体（顺带消除了「两个 device_id 写反」的风险）；`tests/update.rs` 里 `format!` 无插值的 4 处去掉 |
| `screenwall-proto` | 0 | — |

现在 `scripts\ci.ps1` 的 clippy 步也用 `-D warnings`：新引入的 lint 会让本地检查直接失败，
不会等推到 GitHub 才发现。

### 3.3 测试

`cargo test --workspace`：当时 **188 passed / 0 failed**（含能力协商、服务端可观测性、
AEC、连接质量统计、局域网直连集成用例、限流单测与来源校验单测、诊断包脱敏等）。
此后又补了直连来源校验与 `Welcome.caps` 交集等用例，数量以实际输出为准。

### 3.4 前端

- `npx tsc --noEmit`：通过，约 3–8 s。
- `npm run build`（vite 6.4.6）：通过，1618 个模块，约 2–11 s。
  产物 `dist/assets/index-*.js` 约 356 kB（gzip 约 104 kB）。

### 3.5 `scripts\ci.ps1` 完整跑一遍的耗时

一次退出码 0 的完整运行（本机，含前端）：

```
cargo fmt --all -- --check                             PASS     5.3s
cargo clippy --workspace --all-targets -- -D warnings  PASS    14.1s
cargo test --workspace                                 PASS    34.2s
ui npm ci                                              SKIP  (node_modules 已存在)
ui tsc --noEmit                                        PASS     3.4s
ui npm run build                                       PASS     2.6s
ALL CHECKS PASSED  (exit code 0)
```

耗时随机器负载浮动很大：并行编译繁忙时单是 clippy 就到 70 s、test 到 300 s。
以脚本实际打印的耗时为准。

---

## 4. 工作流 YAML 的校验

本机没有 `actionlint`（也没有 Go 工具链可以 `go install`），所以用 Python `PyYAML`
做了结构校验：

```powershell
py -c "import yaml; d=yaml.safe_load(open('.github/workflows/ci.yml',encoding='utf-8')); print(list(d['jobs']))"
# ['fmt', 'build-and-test', 'clippy', 'bundle']
```

这能保证 YAML 语法合法、结构可解析，但**不等于** `actionlint` 的语义检查
（比如表达式 `${{ }}` 的写法、action 输入名拼写）。有条件时建议在推送前跑一次：

```powershell
# 任选其一
winget install rhysd.actionlint      # 若可用
# 或下载 https://github.com/rhysd/actionlint/releases 的 windows_amd64 zip，解压后
.\actionlint.exe .github\workflows\ci.yml
```

该 workflow 已于 2026-11-25 首次在 GitHub 上真实运行：`rustfmt` 通过，`build-and-test` 与 `clippy` 因缺 kaypanel sidecar 失败（见第 5 节）。已给这两个 job 补上 `setup-go` + `build-kaypanel.bat`，等待下一次推送的结果。

---

## 5. 服务器管理 sidecar（kaypanel）与 CI

服务器管理模块的 `kaypanel.exe` 是独立编译的 Go 二进制，**不在 cargo workspace 里**，但它的存在是编译客户端的前提：`crates/client/tauri.conf.json` 的 `bundle.resources` 引用了 `third_party\kaypanel\dist\kaypanel.exe`，而 **Tauri 的 build script 在编译期就会检查这个路径**。所以不只是 `bundle`，`cargo test` / `cargo clippy` 在全新 checkout 上也会因为这个文件缺失而失败（首次线上运行踩到的就是这个坑）。

### 5.1 当前 CI 的实际行为

`.github/workflows/ci.yml` 里 `build-and-test`、`clippy`、`bundle` 三个 job 都先 `actions/setup-go@v5`（固定 `1.23`，按 `go.sum` 缓存），再显式执行：

```yaml
      - name: Build kaypanel sidecar
        shell: pwsh
        run: cmd /c "scripts\build-kaypanel.bat"
```

然后才跑 cargo / 打包。`rustfmt` job 不编译，因此不需要 Go。workflow 级 `env` 把 `GOPROXY` 设成 `https://proxy.golang.org,direct`——脚本默认的 `goproxy.cn` 在国内快，但境外 runner 上官方代理更稳。失败是**阻塞**的（`build-kaypanel.bat` 返回 1），不会产出一个缺面板的安装包。

### 5.2 本地检查与打包

`scripts\ci.ps1` 在跑 clippy / test 之前检查 `third_party\kaypanel\dist\kaypanel.exe`，缺失时自动调用 `build-kaypanel.bat` 补建（需要 Go 1.23+）；已经存在就跳过。所以全新 clone 上一条命令仍能跑通，只是第一次要有 Go。

只出安装包时：

```powershell
scripts\build-kaypanel.bat          # 需要 Go 1.23+，首次会联网拉模块
scripts\tauri-build.bat build --bundles nsis
```

直接跑 `tauri-build.bat` 也可以：它会在 `dist\kaypanel.exe` 缺失时自动调用构建脚本。Go 不在 `PATH` 时用 `GO_BIN` 指定其 `bin` 目录；网络受限时设置 `GOPROXY`。

---

## 6. 常见问题

- **`Blocking waiting for file lock on build directory`**：另一个 cargo 进程正在编译
  （IDE、并行 agent、上一次没退出的 `cargo test`）。等 1–2 分钟重试；必要时结束残留进程：
  ```powershell
  Get-Process screenwall-client,screenwall-server -ErrorAction SilentlyContinue | Stop-Process -Force
  ```
- **`failed to remove file ... os error 5`**：同上，通常是 exe 还在运行导致 `target` 被占用。
- **`vswhere.exe 不是内部或外部命令`**：`scripts\cargo.bat` 在找不到 vswhere 时会回退到
  `%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools`，这条提示是**无害的**，
  cargo 仍能正常构建（本机即如此）。
- **`cargo-fmt.exe is not installed`**：`rustup component add rustfmt`。
  本机初始只装了 `minimal` profile，`rustfmt` 和 `clippy` 是本次补装的。
