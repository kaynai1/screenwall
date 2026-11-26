#!/usr/bin/env bash
#
# 在自建服务器（Linux）上发布一次更新。
#
# publish-update.ps1 是给 Windows 打包机用的；服务器是 Linux 时用这个脚本：
# 把安装包放进 [update].dir，改写 server.toml 的 [update] 段，再重启服务。
# SHA256 不用填 —— 服务器首次请求时会自己算并缓存。
#
# 用法：
#   bash publish-update.sh -v 0.2.8 -f /root/ScreenWall_0.2.8_x64-setup.exe
#
# 可选：
#   --dir <目录>      安装包存放目录，默认 /opt/screenwall/downloads
#   --config <文件>   服务器配置，默认 /opt/screenwall/server.toml
#   --notes <说明>    更新说明，默认 "ScreenWall <版本>"
#   --no-restart      改完不重启服务（自己找时间重启）
#
# 前提：安装包已经传到服务器上（scp 或原来的 Windows 打包机上传）。
set -Eeuo pipefail

APP_DIR=/opt/screenwall
CONFIG="$APP_DIR/server.toml"
DIR="$APP_DIR/downloads"
SERVICE=screenwall
VERSION=""
SOURCE=""
NOTES=""
RESTART=1

usage() {
  sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die() { printf '[x] %s\n' "$*" >&2; exit 1; }
info() { printf '[...] %s\n' "$*"; }
ok() { printf '[ok] %s\n' "$*"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    -v|--version) VERSION="${2:-}"; shift ;;
    -f|--file) SOURCE="${2:-}"; shift ;;
    --dir) DIR="${2:-}"; shift ;;
    --config) CONFIG="${2:-}"; shift ;;
    --notes) NOTES="${2:-}"; shift ;;
    --no-restart) RESTART=0 ;;
    -h|--help) usage; exit 0 ;;
    *) die "未知参数：$1（--help 看用法）" ;;
  esac
  shift
done

if [[ $EUID -eq 0 ]]; then
  SUDO=""
elif command -v sudo >/dev/null 2>&1; then
  SUDO="sudo"
else
  die "需要 root 或 sudo 权限。"
fi

[[ -n "$SOURCE" ]] || die "缺少 -f <安装包路径>"
[[ -f "$SOURCE" ]] || die "找不到安装包：$SOURCE"
FILE=$(basename "$SOURCE")

if [[ -z "$VERSION" ]]; then
  # 从 ScreenWall_0.2.8_x64-setup.exe 里抠版本号。
  VERSION=$(printf '%s' "$FILE" | sed -n 's/^ScreenWall_\([0-9][0-9.]*\)_x64-setup\.exe$/\1/p')
  [[ -n "$VERSION" ]] || die "推不出版本号，请用 -v 指定（文件名建议是 ScreenWall_<版本>_x64-setup.exe）"
fi
[[ -n "$NOTES" ]] || NOTES="ScreenWall $VERSION"

[[ -f "$CONFIG" ]] || die "找不到服务器配置：$CONFIG（--config 指定）"

info "发布 $VERSION：$FILE"
$SUDO mkdir -p "$DIR"
$SUDO cp -f "$SOURCE" "$DIR/$FILE"
ok "安装包已放到 $DIR/$FILE"

# 去掉旧的 [update] 段，再追加新的；其余内容原样保留。
TMP=$(mktemp)
awk '
  /^\[/ { skip = ($0 == "[update]") ? 1 : 0 }
  skip != 1 { print }
' "$CONFIG" > "$TMP"

NOTES_SAFE=$(printf '%s' "$NOTES" | tr '"' "'" | tr '\n' ' ')
cat >> "$TMP" <<EOF

[update]
dir = "$DIR"
version = "$VERSION"
file = "$FILE"
notes = "$NOTES_SAFE"
EOF

$SUDO cp -f "$CONFIG" "$CONFIG.bak.$(date +%Y%m%d%H%M%S)"
$SUDO cp -f "$TMP" "$CONFIG"
rm -f "$TMP"
ok "已写入 [update] 段（旧配置备份为 $CONFIG.bak.*）"

if [[ $RESTART -eq 1 ]]; then
  if command -v systemctl >/dev/null 2>&1; then
    $SUDO systemctl restart "$SERVICE" && ok "已重启 $SERVICE"
  else
    info "没有 systemd，跳过重启；请手动重启服务器让新配置生效。"
  fi
else
  info "按 --no-restart 跳过重启，配置要重启服务器后才生效。"
fi

# 健康检查：从配置里读端口，打个 /api/v1/version 看看版本对不对。
PORT=$(awk -F'"' '/^bind[[:space:]]*=/ { split($2, a, ":"); print a[length(a)]; exit }' "$CONFIG")
if [[ -n "$PORT" ]] && command -v curl >/dev/null 2>&1; then
  sleep 1
  BODY=$(curl -fsS --max-time 5 "http://127.0.0.1:$PORT/api/v1/version" || true)
  if printf '%s' "$BODY" | grep -q "\"$VERSION\""; then
    ok "服务器已在提供 $VERSION"
    printf '     客户端填：http://<服务器IP>:%s\n' "$PORT"
  else
    printf '[!] 接口没返回新版本，检查 server.toml 和日志：journalctl -u %s -n 50\n' "$SERVICE" >&2
    printf '    返回：%s\n' "${BODY:-<空>}" >&2
    exit 1
  fi
fi
