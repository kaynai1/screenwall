#!/usr/bin/env bash
#
# 在自建服务器（Linux）上发布一次更新。
#
# publish-update.ps1 是给 Windows 打包机用的；服务器是 Linux 时用这个脚本：
# 把安装包放进 [update].dir，改写 server.toml 的 [update] 段，再重启服务。
# SHA256 不用填 —— 服务器首次请求时会自己算并缓存。
#
# 用法：
#   bash publish-update.sh                 发布最新正式版（自动下载安装包）
#   bash publish-update.sh -v 0.2.8        发布指定版本（自动下载安装包）
#   bash publish-update.sh -v 0.2.8 -f <路径>   用服务器上已有的安装包
#
# 可选：
#   --dir <目录>      安装包存放目录，默认 /opt/screenwall/downloads
#   --config <文件>   服务器配置，默认 /opt/screenwall/server.toml
#   --notes <说明>    更新说明，默认 "ScreenWall <版本>"
#   --no-restart      改完不重启服务（自己找时间重启）
#
# 安装包默认从 GitHub Releases 下载（先试直链，失败再走 api.github.com），
# 所以服务器上什么都不用准备。
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
  cat <<'USAGE'
在自建服务器（Linux）上发布一次客户端更新。

用法：
  bash publish-update.sh                 发布最新正式版（自动下载安装包）
  bash publish-update.sh -v 0.2.8        发布指定版本（自动下载安装包）
  bash publish-update.sh -v 0.2.8 -f <路径>   用服务器上已有的安装包

可选：
  --dir <目录>      安装包存放目录，默认 /opt/screenwall/downloads
  --config <文件>   服务器配置，默认 /opt/screenwall/server.toml
  --notes <说明>    更新说明，默认 "ScreenWall <版本>"
  --no-restart      改完不重启服务（自己找时间重启）

安装包从 GitHub Releases 自动下载（先试直链，失败再走 api.github.com），
服务器上不需要手动准备任何文件。
USAGE
}

die() { printf '[x] %s\n' "$*" >&2; exit 1; }
info() { printf '[...] %s\n' "$*"; }
ok() { printf '[ok] %s\n' "$*"; }
warn() { printf '[!] %s\n' "$*" >&2; }

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

REPO=kaynai1/screenwall

command -v curl >/dev/null 2>&1 || die "需要 curl：apt install -y curl（或 yum install -y curl）"

# 文件大于 1 MB 才算下载成功，避免把 GitHub 的错误页当成安装包。
big_enough() {
  local size
  size=$(wc -c < "$1" 2>/dev/null | tr -d ' ')
  [[ -n "$size" && "$size" -gt 1000000 ]]
}

# GitHub 上最新的正式版版本号（去掉 v 前缀），失败返回非 0。
latest_version() {
  local json
  json=$(curl -fsSL --max-time 20 -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null) || return 1
  printf '%s' "$json" |
    awk -F'"' '/"tag_name"/ { v = $4; sub(/^v/, "", v); print v }'
}

# 从 release JSON 里找出指定附件名的 asset id。
# GitHub 返回的是带缩进的 JSON：附件对象里 "id" 在 "name" 前面，同为 6 个空格缩进。
asset_id() {
  printf '%s' "$1" | awk -v want="\"name\": \"$2\"" '
    /^      "id": [0-9]+,?$/ { id = $2; sub(/,.*/, "", id) }
    /^      "name": / { if (index($0, want)) { print id; exit } }'
}

# 下载指定版本的安装包；先试 Release 直链，不行再走 api.github.com。
download_installer() {
  local ver="$1" out="$2"
  local name="ScreenWall_${ver}_x64-setup.exe"
  local part="$out.part" url json id

  info "下载 $name（约 9 MB）…"
  url="https://github.com/$REPO/releases/download/v$ver/$name"
  if curl -fL --retry 1 --connect-timeout 8 --max-time 600 -o "$part" "$url" 2>/dev/null &&
     big_enough "$part"; then
    mv -f "$part" "$out"
    return 0
  fi
  rm -f "$part"

  info "直链不通，改走 api.github.com…"
  json=$(curl -fsSL --max-time 20 -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$REPO/releases/tags/v$ver" 2>/dev/null) || return 1
  id=$(asset_id "$json" "$name")
  [[ -n "$id" ]] || return 1
  curl -fL --retry 2 --connect-timeout 10 --max-time 600 -o "$part" \
    -H 'Accept: application/octet-stream' \
    "https://api.github.com/repos/$REPO/releases/assets/$id" 2>/dev/null || { rm -f "$part"; return 1; }
  big_enough "$part" || { rm -f "$part"; return 1; }
  mv -f "$part" "$out"
}

if [[ -n "$SOURCE" ]]; then
  [[ -f "$SOURCE" ]] || die "找不到安装包：$SOURCE"
  FILE=$(basename "$SOURCE")
  if [[ -z "$VERSION" ]]; then
    # 从 ScreenWall_0.2.8_x64-setup.exe 里抠版本号。
    VERSION=$(printf '%s' "$FILE" | sed -n 's/^ScreenWall_\([0-9][0-9.]*\)_x64-setup\.exe$/\1/p')
    [[ -n "$VERSION" ]] || die "推不出版本号，请用 -v 指定（文件名建议是 ScreenWall_<版本>_x64-setup.exe）"
  fi
else
  if [[ -z "$VERSION" ]]; then
    info "查询 GitHub 最新正式版…"
    VERSION=$(latest_version || true)
    [[ -n "$VERSION" ]] || die "取不到最新版本号（服务器可能连不上 GitHub）；用 -v 0.2.8 指定"
  fi
  FILE="ScreenWall_${VERSION}_x64-setup.exe"
  SOURCE="/tmp/$FILE"
  if [[ -f "$SOURCE" ]] && big_enough "$SOURCE"; then
    ok "已有安装包，直接用：$SOURCE"
  else
    download_installer "$VERSION" "$SOURCE" ||
      die "下载安装包失败（服务器连不上 GitHub）；可手动放到 $SOURCE，或用 -f 指定已有文件"
    ok "安装包已下载：$SOURCE"
  fi
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
