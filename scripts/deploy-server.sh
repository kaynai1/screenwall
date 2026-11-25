#!/usr/bin/env bash
#
# ScreenWall 中转服务器 · 一键安装与管理脚本
#
# 用法（在服务器上，用有 sudo 权限的用户）：
#   git clone --depth 1 https://github.com/kaynai1/screenwall.git /tmp/sw \
#     && bash /tmp/sw/scripts/deploy-server.sh
#
# 跑起来是一个菜单，输入编号选择：安装 / 更新 / 状态 / 日志 / 重启 / 备份 / 卸载 / 退出。
# 安装时依次填写：账号名、账号密码、监听端口、是否允许自助注册、确认开始。
# 安装目录、源码地址都用固定默认值，自动处理；装完打印中转服务器地址。
#
set -euo pipefail

# ---------- 固定配置（不再让用户填） ----------
APP_DIR=/opt/screenwall
SRC_DIR=/tmp/screenwall-src
PORT=8787
REPO_URL=https://github.com/kaynai1/screenwall.git
REPO_MIRROR=https://gitclone.com/github.com/kaynai1/screenwall.git
SERVICE=screenwall
CONFIG="$APP_DIR/server.toml"
BIN="$APP_DIR/screenwall-server"
DB="$APP_DIR/screenwall.db"
UNIT=/etc/systemd/system/screenwall.service

export PATH="$HOME/.cargo/bin:$PATH"

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
else
  command -v sudo >/dev/null 2>&1 || die "需要 root 或 sudo 权限。"
  SUDO="sudo"
fi

pause() { read -rp "按回车返回菜单..." _ || true; }

# ---------- 工具函数 ----------
ensure_tools() {
  # /dev/null 必须是正常的字符设备，否则 apt 会报 "cannot create /dev/null: Permission denied"
  if [[ ! -c /dev/null ]]; then
    warn "/dev/null 不是正常的字符设备，apt 会报 Permission denied。"
    warn "请先以 root 执行修复后重跑："
    warn "  chattr -i /dev/null 2>/dev/null; rm -f /dev/null; mknod -m 666 /dev/null c 1 3"
    die "修复 /dev/null 后再来。"
  fi
  info "安装系统依赖…"
  $SUDO apt-get update -y >/dev/null
  $SUDO apt-get install -y git build-essential pkg-config curl ca-certificates gpgv
  ok "系统依赖就绪。"
}

ensure_rust() {
  if command -v cargo >/dev/null 2>&1; then
    ok "已检测到 cargo。"
    return
  fi
  info "安装 Rust（走 rsproxy 镜像）…"
  export RUSTUP_DIST_SERVER=https://rsproxy.cn
  export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
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
  ok "Rust 安装完成。"
}

clone_repo() {
  info "拉取源码…"
  if [[ -d "$SRC_DIR/.git" ]]; then
    git -C "$SRC_DIR" fetch --depth 1 && git -C "$SRC_DIR" reset --hard origin/main
  else
    rm -rf "$SRC_DIR"
    if ! git clone --depth 1 "$REPO_URL" "$SRC_DIR" 2>/dev/null; then
      warn "GitHub 直连失败，改用镜像 $REPO_MIRROR"
      rm -rf "$SRC_DIR"
      git clone --depth 1 "$REPO_MIRROR" "$SRC_DIR"
    fi
  fi
  ok "源码就绪。"
}

build_server() {
  info "编译 screenwall-server（2 核约 3–5 分钟）…"
  (cd "$SRC_DIR" && cargo build --release -p screenwall-server)
  ok "编译完成。"
}

# 从 server.toml 读出实际监听端口；读不到则回退 8787。
config_port() {
  sed -n 's/^bind *= *"[^"]*:\([0-9][0-9]*\)".*/\1/p' "$CONFIG" 2>/dev/null | head -1
}

# 取服务器对外地址：优先公网 IP，取不到用本机 IP，再不行 127.0.0.1。
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
  p=$(config_port); p=${p:-8787}
  sleep 1
  local h=""
  if command -v curl >/dev/null 2>&1; then
    h=$(curl -fsS "http://127.0.0.1:$p/healthz" 2>/dev/null || true)
  fi
  if [[ -n "$h" ]]; then
    ok "健康检查：$h"
  else
    warn "健康检查未通过，可用「查看日志」排查。"
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

# ---------- 1. 安装 ----------
install_server() {
  echo
  if [[ -f "$CONFIG" ]]; then
    warn "检测到已安装（$CONFIG 存在）。"
    read -rp "仍要重新安装吗? (y/N): " ans
    [[ "${ans,,}" != "y" ]] && return
  fi

  # 1. 账号名
  read -rp "账号名（登录用，如 kay）: " ACCOUNT
  [[ -z "$ACCOUNT" ]] && { warn "账号名不能为空。"; return; }
  # 2. 账号密码
  read -rsp "账号密码（≥8 位，输入不显示）: " PASSWORD; echo
  [[ -z "$PASSWORD" ]] && { warn "密码不能为空。"; return; }
  [[ ${#PASSWORD} -lt 8 ]] && warn "密码少于 8 位，建议更长。"
  # 3. 监听端口
  read -rp "监听端口 [8787]: " PORT; PORT=${PORT:-8787}
  # 4. 允许自助注册
  read -rp "允许客户端自助注册账号? (y/N): " REGISTER; REGISTER=${REGISTER:-N}

  # 5. 确认开始部署
  echo
  info "配置确认："
  printf '   账号        : %s\n' "$ACCOUNT"
  printf '   端口        : %s\n' "$PORT"
  printf '   自助注册    : %s\n' "$([ "$REGISTER" = "y" ] && echo 是 || echo 否)"
  echo
  read -rp "确认开始部署? (Y/n): " CONFIRM
  [[ "${CONFIRM,,}" == "n" ]] && { info "已取消。"; return; }

  ensure_tools
  ensure_rust
  clone_repo
  build_server

  info "安装到 $APP_DIR 并写配置…"
  $SUDO mkdir -p "$APP_DIR"
  $SUDO cp "$SRC_DIR/target/release/screenwall-server" "$BIN"
  $SUDO chmod 755 "$BIN"
  local reg_bool
  reg_bool=$([ "$REGISTER" = "y" ] && echo true || echo false)
  $SUDO tee "$CONFIG" >/dev/null <<EOF
# ScreenWall 服务器配置（由 deploy-server.sh 生成）
bind = "0.0.0.0:$PORT"
db_path = "$DB"
allow_registration = $reg_bool
token_ttl_days = 30
max_frame_bytes = 1048576
EOF
  ok "配置已写入 $CONFIG"

  info "创建账号 $ACCOUNT …"
  if $SUDO "$BIN" --config "$CONFIG" user add "$ACCOUNT" --password "$PASSWORD"; then
    ok "账号 $ACCOUNT 已创建。"
  else
    warn "账号 $ACCOUNT 可能已存在；改密码： $BIN --config $CONFIG user reset $ACCOUNT"
  fi
  unset PASSWORD

  write_unit
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable "$SERVICE"
  $SUDO systemctl restart "$SERVICE"
  ok "服务已启动并设为开机自启。"

  echo
  if command -v ufw >/dev/null 2>&1 && $SUDO ufw status 2>/dev/null | grep -q "Status: active"; then
    $SUDO ufw allow "$PORT"/tcp
    ok "已用 ufw 放行端口 $PORT。"
  else
    warn "未检测到活跃 ufw。请到云控制台【安全组】放行 TCP 端口 $PORT。"
  fi

  health_check
  local ip
  ip=$(public_ip)
  echo
  ok "部署完成！"
  echo "  中转服务器地址： http://$ip:$PORT"
  echo "  在 ScreenWall 设置 → 连接 → 服务器地址填上面的地址，用 $ACCOUNT 登录。"
}

# ---------- 2. 更新 ----------
update_server() {
  echo
  [[ -f "$CONFIG" ]] || { warn "尚未安装，请先选 1 安装。"; return; }
  ensure_tools
  ensure_rust
  clone_repo
  build_server
  info "替换二进制（旧版备份为 screenwall-server.bak）…"
  $SUDO cp "$BIN" "$BIN.bak" 2>/dev/null || true
  $SUDO cp "$SRC_DIR/target/release/screenwall-server" "$BIN"
  $SUDO chmod 755 "$BIN"
  $SUDO systemctl restart "$SERVICE"
  ok "已更新并重启。"
  health_check
}

# ---------- 3. 状态 ----------
show_status() {
  echo
  if ! systemctl list-unit-files 2>/dev/null | grep -q "^$SERVICE.service"; then
    warn "服务未安装。"
    return
  fi
  $SUDO systemctl status "$SERVICE" --no-pager -l || true
}

# ---------- 4. 日志 ----------
show_logs() {
  echo
  $SUDO journalctl -u "$SERVICE" -e --no-pager || true
}

# ---------- 5. 重启 ----------
restart_server() {
  echo
  $SUDO systemctl restart "$SERVICE"
  ok "已重启。"
  health_check
}

# ---------- 6. 备份 ----------
backup_data() {
  echo
  [[ -f "$DB" ]] || { warn "数据库不存在：$DB"; return; }
  local dir="$APP_DIR/backups"
  local file="$dir/screenwall-$(date +%Y%m%d-%H%M%S).db"
  $SUDO mkdir -p "$dir"
  $SUDO cp "$DB" "$file"
  ok "已备份到 $file"
}

# ---------- 7. 卸载 ----------
uninstall_server() {
  echo
  warn "这会停止并移除服务。"
  read -rp "确认卸载? (y/N): " ans
  [[ "${ans,,}" != "y" ]] && return
  $SUDO systemctl stop "$SERVICE" 2>/dev/null || true
  $SUDO systemctl disable "$SERVICE" 2>/dev/null || true
  $SUDO rm -f "$UNIT"
  $SUDO systemctl daemon-reload
  ok "服务已移除。"
  read -rp "是否删除数据目录 $APP_DIR（含数据库）? (y/N): " rmdata
  if [[ "${rmdata,,}" == "y" ]]; then
    $SUDO rm -rf "$APP_DIR"
    ok "已删除 $APP_DIR"
  else
    info "保留 $APP_DIR（含数据库）。"
  fi
}

# ---------- 主菜单 ----------
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

main_menu
