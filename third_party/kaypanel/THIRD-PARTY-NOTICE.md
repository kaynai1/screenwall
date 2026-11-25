# 第三方组件声明：KayPanel

ScreenWall 的「服务器管理」模块内置了 [KayPanel](https://github.com/kaynai1/kaypanel)，
以 sidecar 子进程方式随安装包分发。

- 上游项目：https://github.com/kaynai1/kaypanel
- 版本：v0.14.1
- 许可：MIT License（见上游 README「许可」一节）
- 本项目改动：
  1. `main.go` 增加 `-sidecar` 模式：只监听 `127.0.0.1:0`（随机端口），
     启动后向 stdout 输出一行握手 JSON（地址 + 一次性登录令牌 + PID），
     stdin 读到 EOF 时优雅退出。
  2. 桌面版（`desktop/`）与 Docker 相关文件原样保留，但不参与 ScreenWall 的打包。

未修改 upstream 的任何业务逻辑、鉴权逻辑与前端资源。`go.sum` 一并 vendor，
构建可复现。

## 构建

```bat
scripts\build-kaypanel.bat
```

产物：`third_party\kaypanel\dist\kaypanel.exe`，由 `crates/client/tauri.conf.json`
的 `bundle.resources` 打进安装包。
