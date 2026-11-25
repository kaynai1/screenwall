; ScreenWall 安装/卸载钩子：给局域网直连放行入站 TCP。
;
; 规则只放行 ScreenWall 自己的可执行文件（program=），不是对整个端口开放。
; 规则名必须与 crates/client/src/firewall.rs 的 RULE_NAME 完全一致。
;
; 端口写成默认值 47900：安装器读不到用户的 client.json。用户改过端口、
; 规则被删除、或者从 currentUser 安装升级过来时，由应用内
; 「允许局域网直连」按钮提权修复（同样的规则名，先删后加）。

!define SCREENWALL_FIREWALL_RULE "ScreenWall Direct"
!define SCREENWALL_FIREWALL_PORT "47900"
; 自动发现：同网段主控端广播 UDP 探测，被控端在 47901 上应答。
!define SCREENWALL_DISCOVERY_RULE "ScreenWall Discovery"
!define SCREENWALL_DISCOVERY_PORT "47901"

!macro NSIS_HOOK_POSTINSTALL
  ; POSTINSTALL 时 exe 已经落盘，program= 可以指向真实路径。
  ; 先删后加：升级/换安装目录后旧规则的 program= 会失效，同名的旧规则
  ; 也会让应用内的规则检测变得不确定。
  nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name="${SCREENWALL_FIREWALL_RULE}"'
  Pop $0
  Pop $1
  nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name="${SCREENWALL_DISCOVERY_RULE}"'
  Pop $0
  Pop $1

  nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall add rule name="${SCREENWALL_FIREWALL_RULE}" dir=in action=allow protocol=TCP localport=${SCREENWALL_FIREWALL_PORT} program="$INSTDIR\${MAINBINARYNAME}.exe" profile=domain,private enable=yes'
  Pop $0
  Pop $1
  ${If} $0 != 0
    ; 不阻断安装：currentUser 安装或组策略禁止时，应用内还有一次提权兜底。
    DetailPrint "ScreenWall: 添加防火墙规则失败（$0）: $1"
  ${Else}
    DetailPrint "ScreenWall: 已添加防火墙规则 ${SCREENWALL_FIREWALL_RULE}（TCP ${SCREENWALL_FIREWALL_PORT}）"
  ${EndIf}

  nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall add rule name="${SCREENWALL_DISCOVERY_RULE}" dir=in action=allow protocol=UDP localport=${SCREENWALL_DISCOVERY_PORT} program="$INSTDIR\${MAINBINARYNAME}.exe" profile=domain,private enable=yes'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "ScreenWall: 添加自动发现规则失败（$0）: $1"
  ${Else}
    DetailPrint "ScreenWall: 已添加防火墙规则 ${SCREENWALL_DISCOVERY_RULE}（UDP ${SCREENWALL_DISCOVERY_PORT}）"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; 卸载（含升级前的旧版卸载）时清掉规则；删不存在的规则返回非 0，忽略。
  ; 用 PREUNINSTALL 而不是 POSTUNINSTALL：此时 exe 还在，语义上更接近
  ; 「这个程序不再需要入站」，且卸载器本身是提权的。
  nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name="${SCREENWALL_FIREWALL_RULE}"'
  Pop $0
  Pop $1
  nsExec::ExecToStack '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name="${SCREENWALL_DISCOVERY_RULE}"'
  Pop $0
  Pop $1
!macroend
