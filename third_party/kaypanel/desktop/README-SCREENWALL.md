# 这个目录在 ScreenWall 里不参与构建

`desktop/` 是 KayPanel 上游自带的 WebView2 桌面外壳（自带窗口、托盘、
单实例唤醒）。ScreenWall 不用它，原因：

- ScreenWall 需要把面板**嵌在自己的主窗口里**（子 webview），
  而 KayPanel 的桌面外壳会另开一个顶层窗口。
- 外壳里的托盘、单实例互斥量（`Local\KayPanelDesktop`）会和
  ScreenWall 自己的托盘/单实例逻辑冲突。

ScreenWall 只用**服务端二进制**：

```bat
go build -trimpath -ldflags "-s -w -H windowsgui -X main.version=0.14.1" -o dist\kaypanel.exe .
```

即仓库根的 `main.go`（`package main`），配合 `-sidecar` 参数运行。

保留这个目录只是为了 vendor 完整性，方便将来对照上游。它不会被
`scripts\build-kaypanel.bat` 编译（那个脚本构建的是仓库根目录的包）。
