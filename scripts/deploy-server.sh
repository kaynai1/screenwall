#!/usr/bin/env bash
#
# ScreenWall 中转服务器 · 一键安装与管理脚本
#
# 交互安装（推荐先看菜单）：
#   bash deploy-server.sh
#
# 完全无交互的一键安装（适合复制一条命令就跑）：
#   bash deploy-server.sh --yes --account kay --password '至少8位密码'
#   可选：--port 8787  --allow-register
#
# 脚本会自动：修复异常的 /dev/null、安装依赖（失败自动重试）、安装 Rust
# （走国内镜像）、下载源码（tarball 优先，git 兜底）、编译并注册 systemd、
# 放行防火墙端口，最后打印中转地址。
#
set -Eeuo pipefail

# ---------- 固定配置（不再让用户填） ----------
APP_DIR=/opt/screenwall
SRC_DIR=/tmp/screenwall-src
DEFAULT_PORT=8787
REPO_OWNER=kaynai1
REPO_NAME=screenwall
REPO_URL="https://github.com/$REPO_OWNER/$REPO_NAME.git"
REPO_MIRROR="https://gitclone.com/github.com/$REPO_OWNER/$REPO_NAME.git"
TARBALL="https://codeload.github.com/$REPO_OWNER/$REPO_NAME/tar.gz/refs/heads/main"
TARBALL_MIRROR="https://gitclone.com/github.com/$REPO_OWNER/$REPO_NAME/archive/refs/heads/main.tar.gz"
SERVICE=screenwall
CONFIG="$APP_DIR/server.toml"
BIN="$APP_DIR/screenwall-server"
DB="$APP_DIR/screenwall.db"
UNIT=/etc/systemd/system/screenwall.service

export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_NET_RETRY=5

# ---------- 参数 ----------
YES=0
ACCOUNT=""
PASSWORD=""
PORT=""
REGISTER=""

usage() {
  cat <<'USAGE'
用法：
  bash deploy-server.sh                     打开交互菜单
  bash deploy-server.sh --yes --account 账号 --password 密码 [--port 8787] [--allow-register]
                                            无交互一键安装
  bash deploy-server.sh --help              显示本帮助
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -y|--yes) YES=1 ;;
    --account) ACCOUNT="${2:-}"; shift ;;
    --password) PASSWORD="${2:-}"; shift ;;
    --port) PORT="${2:-}"; shift ;;
    --allow-register) REGISTER="y" ;;
    -h|--help) usage; exit 0 ;;
    *) printf '未知参数：%s\n' "$1" >&2; usage; exit 2 ;;
  esac
  shift
done

# ---------- 颜色 ----------
if [[ -t 1 ]]; then
  C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_YEL=$'\033[33m'; C_BLU=$'\033[36m'; C_OFF=$'\033[0m'
else
  C_RED=''; C_GRN=''; C_YEL=''; C_BLU=''; C_OFF=''
fi
info() { printf '%s[·]%s %s\n' "$C_BLU" "$C_OFF" "$*"; }
ok()   { printf '%s[✓]%s %s\n' "$C_GRN" "$C_OFF" "$*"; }
warn() { printf '%s[!]%s %s\n' "$C_YEL" "$C_OFF" "$*"; }
die()  { printf '%s[✗]%s %s\n' "$C_RED" "$C_OFF" "$*" >&2; exit 1; }

# ---------- 权限 ----------
if [[ $EUID -eq 0 ]]; then
  SUDO=""
elif command -v sudo >/dev/null 2>&1; then
  SUDO="sudo"
else
  die "需要 root 或 sudo 权限。"
fi

pause() { [[ $YES -eq 1 ]] && return 0; read -rp "按回车返回菜单..." _ || true; }

# ---------- 自检与修复 ----------
# /dev/null 被换成了普通文件/软链接时，apt、gpgv、cargo 全都会报
# "cannot create /dev/null: Permission denied"，这是最常见的部署失败原因。
ensure_devnull() {
  [[ -c /dev/null ]] && return 0
  warn "/dev/null 不正常：$(ls -l /dev/null 2>/dev/null || echo '不存在')"
  warn "这会让 apt / gpgv 全部失败，先自动修复。"
  # 已存在的话先备份成普通文件，便于事后排查（/dev/null 若是软链接，cp 会跟随）。
  if [[ -e /dev/null && ! -L /dev/null ]]; then
    $SUDO cp -f /dev/null "/tmp/dev-null.bak.$$" 2>/dev/null || true
  fi
  $SUDO chattr -i /dev/null 2>/dev/null || true
  $SUDO rm -f /dev/null
  $SUDO mknod -m 666 /dev/null c 1 3 || true
  if [[ -c /dev/null ]]; then
    ok "已修复 /dev/null"
  else
    die "修复 /dev/null 失败，请手动执行：chattr -i /dev/null; rm -f /dev/null; mknod -m 666 /dev/null c 1 3"
  fi
}

apt_install() {
  local packages=("$@")
  command -v apt-get >/dev/null 2>&1 || { warn "没有 apt-get，跳过系统依赖安装。"; return 0; }
  info "安装系统依赖：${packages[*]}"
  # update 失败多半是镜像源问题，不阻断：只要 install 能成就行。
  $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -y >/dev/null 2>&1 \
    || warn "apt-get update 有失败项（镜像源可能不全），继续尝试安装。"
  local attempt
  for attempt in 1 2 3; do
    if $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${packages[@]}"; then
      ok "系统依赖就绪。"
      return 0
    fi
    warn "依赖安装第 $attempt 次失败，2 秒后重试…"
    sleep 2
  done
  warn "系统依赖安装失败；若所需命令其实已存在，会继续。"
}

ensure_tools() {
  ensure_devnull
  local missing=()
  command -v curl >/dev/null 2>&1 || missing+=(curl)
  command -v tar >/dev/null 2>&1 || missing+=(tar)
  command -v git >/dev/null 2>&1 || missing+=(git)
  command -v cc >/dev/null 2>&1 || missing+=(build-essential)
  command -v pkg-config >/dev/null 2>&1 || missing+=(pkg-config)
  command -v gpgv >/dev/null 2>&1 || missing+=(gpgv)
  if [[ ${#missing[@]} -gt 0 ]]; then
    apt_install ca-certificates "${missing[@]}"
  fi
  command -v curl >/dev/null 2>&1 || die "缺少 curl，无法继续。"
  command -v tar >/dev/null 2>&1 || die "缺少 tar，无法继续。"
  command -v cc >/dev/null 2>&1 || warn "缺少 C 编译器，编译 rusqlite 可能失败。"
}

ensure_rust() {
  if command -v cargo >/dev/null 2>&1; then
    ok "已检测到 cargo（$(cargo --version 2>/dev/null || echo '版本未知')）。"
    return 0
  fi
  info "安装 Rust（走 rsproxy 镜像）…"
  export RUSTUP_DIST_SERVER=https://rsproxy.cn
  export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable \
    || die "Rust 安装失败，请检查网络后重试。"
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
  mkdir -p "$HOME/.cargo"
  cat > "$HOME/.cargo/config.toml" <<'CFG'
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
retry = 5
CFG
  command -v cargo >/dev/null 2>&1 || die "Rust 安装了但找不到 cargo。"
  ok "Rust 安装完成。"
}

# 源码优先用 tarball：不依赖 git，也不用处理浅克隆的历史问题。
fetch_source() {
  info "下载源码…"
  rm -rf "$SRC_DIR"
  mkdir -p "$SRC_DIR"
  local url
  for url in "$TARBALL" "$TARBALL_MIRROR"; do
    if curl -fsSL --retry 2 --connect-timeout 10 --max-time 180 "$url" -o "$SRC_DIR.tar.gz"; then
      tar -xzf "$SRC_DIR.tar.gz" -C "$SRC_DIR" --strip-components=1 && rm -f "$SRC_DIR.tar.gz"
      ok "源码就绪。"
      return 0
    fi
    warn "下载失败：$url"
  done
  # 最后再试 git。
  if command -v git >/dev/null 2>&1; then
    rm -rf "$SRC_DIR"
    for url in "$REPO_URL" "$REPO_MIRROR"; do
      if git clone --depth 1 "$url" "$SRC_DIR" 2>/dev/null; then
        ok "源码就绪（git）。"
        return 0
      fi
      warn "git clone 失败：$url"
    done
  fi
  die "下载源码失败，请检查服务器网络（GitHub 可能被阻断）。"
}

# 小内存云主机编译 Rust 可能被 OOM 杀掉；没有 swap 就补一块。
ensure_swap() {
  local mem_kb swaps
  mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
  swaps=$(awk '/SwapTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
  if (( mem_kb > 0 && mem_kb < 1800000 && swaps < 262144 )) && [[ ! -e /swapfile ]]; then
    warn "内存不足 2 GB 且没有 swap，创建 2 GB swap 供编译使用…"
    if $SUDO fallocate -l 2G /swapfile 2>/dev/null || $SUDO dd if=/dev/zero of=/swapfile bs=1M count=2048 status=none; then
      $SUDO chmod 600 /swapfile
      $SUDO mkswap /swapfile >/dev/null 2>&1
      $SUDO swapon /swapfile 2>/dev/null && ok "swap 已启用。" || warn "swap 启用失败，编译可能较慢。"
    else
      warn "创建 swap 失败，继续尝试编译。"
    fi
  fi
}

build_server() {
  ensure_swap
  local mem_kb jobs=()
  mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
  # 4 GB 以下限制并发，避免 rustc 被 OOM 杀掉。
  if (( mem_kb > 0 && mem_kb < 4000000 )); then
    jobs=(-j 2)
  fi
  info "编译 screenwall-server（约 3–5 分钟）…"
  ( cd "$SRC_DIR" && cargo build --release -p screenwall-server ${jobs[@]+"${jobs[@]}"} ) \
    || die "编译失败。看上方第一处 error；常见原因是内存不足（可加 swap）或网络拉不到依赖。"
  ok "编译完成。"
}

config_port() {
  sed -n 's/^bind *= *"[^"]*:\([0-9][0-9]*\)".*/\1/p' "$CONFIG" 2>/dev/null | head -1
}

public_ip() {
  local ip=""
  if command -v curl >/dev/null 2>&1; then
    ip=$(curl -fsS --max-time 5 https://api.ipify.org 2>/dev/null || true)
  fi
  if [[ -z "$ip" ]]; then
    ip=$(hostname -I 2>/dev/null | awk '{print $1}')
  fi
  echo "${ip:-127.0.0.1}"
}

health_check() {
  local p
  p=$(config_port); p=${p:-$DEFAULT_PORT}
  sleep 1
  local h=""
  if command -v curl >/dev/null 2>&1; then
    h=$(curl -fsS "http://127.0.0.1:$p/healthz" 2>/dev/null || true)
  fi
  if [[ -n "$h" ]]; then
    ok "健康检查：$h"
  else
    warn "健康检查未通过，请用「查看日志」排查（journalctl -u $SERVICE -e）。"
  fi
}

write_unit() {
  $SUDO tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=ScreenWall server
After=network.target

[Service]
Type=simple
ExecStart=$BIN run --config $CONFIG
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
}

open_firewall() {
  local port="$1"
  if command -v ufw >/dev/null 2>&1 && $SUDO ufw status 2>/dev/null | grep -q "Status: active"; then
    $SUDO ufw allow "$port"/tcp >/dev/null 2>&1 && ok "已用 ufw 放行端口 $port。"
  else
    warn "未检测到活跃 ufw。请到云控制台【安全组】放行 TCP 端口 $port。"
  fi
}

# 实际部署：账号/密码/端口已就绪。
do_install() {
  local account="$1" password="$2" port="$3" register="$4"
  [[ -n "$account" ]] || die "账号名不能为空。"
  [[ ${#password} -ge 8 ]] || die "密码至少 8 位。"
  [[ "$port" =~ ^[0-9]+$ ]] && (( port > 0 && port < 65536 )) || die "端口不合法：$port"

  ensure_tools
  ensure_rust
  fetch_source
  build_server

  info "安装到 $APP_DIR 并写配置…"
  $SUDO mkdir -p "$APP_DIR"
  $SUDO cp "$SRC_DIR/target/release/screenwall-server" "$BIN"
  $SUDO chmod 755 "$BIN"
  local reg_bool
  reg_bool=$([ "$register" = "y" ] && echo true || echo false)
  $SUDO tee "$CONFIG" >/dev/null <<EOF
# ScreenWall 服务器配置（由 deploy-server.sh 生成）
bind = "0.0.0.0:$port"
db_path = "$DB"
allow_registration = $reg_bool
token_ttl_days = 30
max_frame_bytes = 1048576
EOF
  ok "配置已写入 $CONFIG"

  info "创建账号 $account …"
  if $SUDO "$BIN" --config "$CONFIG" user add "$account" --password "$password"; then
    ok "账号 $account 已创建。"
  else
    warn "账号 $account 可能已存在；改密码：$BIN --config $CONFIG user reset $account"
  fi

  if ! command -v systemctl >/dev/null 2>&1; then
    die "这台机器没有 systemd（常见于容器/WSL）。可手动前台运行：$BIN run --config $CONFIG"
  fi
  write_unit
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable "$SERVICE" >/dev/null 2>&1 || true
  $SUDO systemctl restart "$SERVICE"
  ok "服务已启动并设为开机自启。"

  open_firewall "$port"
  health_check
  local ip
  ip=$(public_ip)
  echo
  ok "部署完成！"
  echo "  中转服务器地址： http://$ip:$port"
  echo "  账号： $account"
  echo "  在 ScreenWall 登录页填服务器地址与账号即可（不要进局域网模式）。"
}

# ---------- 交互式安装 ----------
install_server() {
  echo
  if [[ -f "$CONFIG" ]]; then
    warn "检测到已安装（$CONFIG 存在）。"
    read -rp "仍要重新安装吗? (y/N): " ans
    [[ "${ans,,}" != "y" ]] && return
  fi

  read -rp "账号名（登录用，如 kay）: " ACCOUNT
  [[ -z "$ACCOUNT" ]] && { warn "账号名不能为空。"; return; }
  read -rsp "账号密码（≥8 位，输入不显示）: " PASSWORD; echo
  [[ -z "$PASSWORD" ]] && { warn "密码不能为空。"; return; }
  [[ ${#PASSWORD} -lt 8 ]] && { warn "密码至少 8 位。"; return; }
  local port
  read -rp "监听端口 [$DEFAULT_PORT]: " port; port=${port:-$DEFAULT_PORT}
  local register
  read -rp "允许客户端自助注册账号? (y/N): " register; register=${register:-N}

  echo
  info "配置确认："
  printf '   账号        : %s\n' "$ACCOUNT"
  printf '   端口        : %s\n' "$port"
  printf '   自助注册    : %s\n' "$([ "$register" = "y" ] && echo 是 || echo 否)"
  echo
  read -rp "确认开始部署? (Y/n): " CONFIRM
  [[ "${CONFIRM,,}" == "n" ]] && { info "已取消。"; return; }

  do_install "$ACCOUNT" "$PASSWORD" "$port" "$register"
  PASSWORD=""
}

# ---------- 管理 ----------
update_server() {
  echo
  [[ -f "$CONFIG" ]] || { warn "尚未安装，请先选 1 安装。"; return; }
  ensure_tools
  ensure_rust
  fetch_source
  build_server
  info "替换二进制（旧版备份为 screenwall-server.bak）…"
  $SUDO cp "$BIN" "$BIN.bak" 2>/dev/null || true
  $SUDO cp "$SRC_DIR/target/release/screenwall-server" "$BIN"
  $SUDO chmod 755 "$BIN"
  $SUDO systemctl restart "$SERVICE"
  ok "已更新并重启。"
  health_check
}

show_status() {
  echo
  if ! systemctl list-unit-files 2>/dev/null | grep -q "^$SERVICE.service"; then
    warn "服务未安装。"
    return
  fi
  $SUDO systemctl status "$SERVICE" --no-pager -l || true
}

show_logs() {
  echo
  $SUDO journalctl -u "$SERVICE" -e --no-pager || true
}

restart_server() {
  echo
  $SUDO systemctl restart "$SERVICE"
  ok "已重启。"
  health_check
}

backup_data() {
  echo
  [[ -f "$DB" ]] || { warn "数据库不存在：$DB"; return; }
  local dir="$APP_DIR/backups"
  local file="$dir/screenwall-$(date +%Y%m%d-%H%M%S).db"
  $SUDO mkdir -p "$dir"
  $SUDO cp "$DB" "$file"
  ok "已备份到 $file"
}

uninstall_server() {
  echo
  warn "这会停止并移除服务。"
  local ans
  read -rp "确认卸载? (y/N): " ans
  [[ "${ans,,}" != "y" ]] && return
  $SUDO systemctl stop "$SERVICE" 2>/dev/null || true
  $SUDO systemctl disable "$SERVICE" 2>/dev/null || true
  $SUDO rm -f "$UNIT"
  $SUDO systemctl daemon-reload
  ok "服务已移除。"
  local rmdata
  read -rp "是否删除数据目录 $APP_DIR（含数据库）? (y/N): " rmdata
  if [[ "${rmdata,,}" == "y" ]]; then
    $SUDO rm -rf "$APP_DIR"
    ok "已删除 $APP_DIR"
  else
    info "保留 $APP_DIR（含数据库）。"
  fi
}

main_menu() {
  while true; do
    echo
    printf '%s\n' "${C_BLU}════════ ScreenWall 中转服务器 ════════${C_OFF}"
    cat <<'MENU'
 1. 安装（SQLite 单机，最简单）
 2. 更新到最新版
 3. 查看状态
 4. 查看日志
 5. 重启服务
 6. 备份数据
 7. 卸载
 0. 退出
MENU
    printf '%s\n' "${C_BLU}═══════════════════════════════════════${C_OFF}"
    read -rp "请输入编号: " choice || exit 0
    case "$choice" in
      1) install_server ;;
      2) update_server ;;
      3) show_status ;;
      4) show_logs ;;
      5) restart_server ;;
      6) backup_data ;;
      7) uninstall_server ;;
      0) exit 0 ;;
      *) warn "无效选择：$choice" ;;
    esac
    pause
  done
}

# ---------- 入口 ----------
if [[ $YES -eq 1 || -n "$ACCOUNT" ]]; then
  [[ -n "$ACCOUNT" ]] || die "无交互安装需要 --account 账号 --password 密码。"
  [[ -n "$PASSWORD" ]] || die "无交互安装需要 --password 密码。"
  do_install "$ACCOUNT" "$PASSWORD" "${PORT:-$DEFAULT_PORT}" "${REGISTER:-N}"
  exit 0
fi

main_menu
