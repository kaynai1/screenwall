# usbmmidd_v2（第三方虚拟显示器驱动）

本目录是 Amyuni Technologies 的 **USB Mobile Monitor v2**（间接显示驱动，IDD）原样副本，
用于无显示器（headless）的被控端：临时接一块虚拟屏，让 DXGI/GDI 有东西可采集。

- 上游：<https://www.amyuni.com/downloads/usbmmidd_v2.zip>
- 版权：Copyright 2014-2021 Amyuni Technologies Inc.
- 许可：见同目录 `License.txt`。允许再分发，但必须保留版权声明、不得声称是原创，
  且使用时会弹一次不受作者控制的广告页。我们未修改任何文件（`usbmmIdd.inf` 一旦改动
  数字签名即失效）。
- 签名：驱动经微软签名；重复安装会重新要求确认。
- 用法：由 ScreenWall 在检测到本机没有任何活动显示输出、且用户开启了「无头时接入虚拟
  显示器」时，以管理员身份执行 `deviceinstaller64.exe install usbmmidd.inf usbmmidd`
  和 `enableidd 1`；卸载时先 `enableidd 0`，再 `stop` / `remove`。
  安装成功会写 `HKLM\Software\ScreenWall\UsbmmiddDriverInstalled = 1`，
  NSIS 卸载钩子只在这个标记存在时才删除驱动。

本仓库代码不修改、不重新打包该驱动；`deviceinstaller*.exe`、`usbmmIdd.*`、`*.cat`
均按原样随安装包分发。
