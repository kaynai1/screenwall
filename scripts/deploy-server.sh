#!/usr/bin/env bash
#
# ScreenWall 中转服务器 · 交互式一键部署脚本
#
# 一条命令跑起来后，会按提示逐项填写（账号、密码、端口等），
# 填完自动完成：装依赖 → 装 Rust → 编译 server → 写配置 → 建账号 → 装 systemd → 提示放行端口。
#
# 用法（在服务器上，用有 sudo 权限的用户）：
#   git clone --depth 1 https://github.com/kaynai1/screenwall.git /tmp/sw \
#     && bash /tmp/sw/scripts/deploy-server.sh
#
#   国内服务器 GitHub 不通时，换镜像地址（脚本里也会让你填）：
#   git clone --depth 1 https://gitclone.com/github.com/kaynai1/screenwall.git /tmp/sw \
#     && bash /tmp/sw/scripts/deploy-server.sh
#
set -euo pipefail

# 保证 cargo 在 PATH 里（无论是否新装）
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
  command -v sudo >/dev/null 2>&1 || die "需要 root 或 sudo 权限，请用 root 用户运行或给用户加 sudo。"
  SUDO="sudo"
fi

# ---------- 交互式收集配置 ----------
printf '\n%s\n' "${C_BLU}════════ ScreenWall 中转服务器部署 ════════${C_OFF}"
printf '%s\n' "按提示填写，直接回车用默认值。\n"

read -rp "账号名（登录用，如 kay）: " ACCOUNT
[[ -z "$ACCOUNT" ]] && die "账号名不能为空"

read -rsp "账号密码（输入不显示，≥8 位）: " PASSWORD; echo
[[ -z "$PASSWORD" ]] && die "密码不能为空"
[[ ${#PASSWORD} -lt 8 ]] && warn "密码少于 8 位，建议用更长的密码"

read -rp "监听端口 [8787]: " PORT; PORT=${PORT:-8787}
read -rp "安装目录 [/opt/screenwall]: " APP_DIR; APP_DIR=${APP_DIR:-/opt/screenwall}
read -rp "源码临时目录 [/tmp/screenwall-src]: " SRC_DIR; SRC_DIR=${SRC_DIR:-/tmp/screenwall-src}
read -rp "源码仓库地址 [https://github.com/kaynai1/screenwall.git]: " REPO_URL; REPO_URL=${REPO_URL:-https://github.com/kaynai1/screenwall.git}
read -rp "允许客户端自助注册账号? (y/N): " REGISTER; REGISTER=${REGISTER:-N}

echo
info "配置确认："
printf '   账号        : %s\n' "$ACCOUNT"
printf '   端口        : %s\n' "$PORT"
printf '   安装目录    : %s\n' "$APP_DIR"
printf '   源码目录    : %s\n' "$SRC_DIR"
printf '   源码地址    : %s\n' "$REPO_URL"
printf '   自助注册    : %s\n' "$([ "$REGISTER" = "y" ] && echo 是 || echo 否)"
echo
read -rp "确认开始部署? (Y/n): " CONFIRM
[[ "${CONFIRM,,}" == "n" ]] && { info "已取消。"; exit 0; }

# ---------- 1. 系统依赖 ----------
info "1/7 安装系统依赖…"
$SUDO apt-get update -y
$SUDO apt-get install -y git build-essential pkg-config curl ca-certificates
ok "系统依赖就绪。"

# ---------- 2. Rust ----------
info "2/7 安装 Rust 工具链…"
if command -v cargo >/dev/null 2>&1; then
  ok "已检测到 cargo，跳过。"
else
  # 国内镜像加速 rustup 与 crates 拉取
  export RUSTUP_DIST_SERVER=https://rsproxy.cn
  export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
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
fi

# ---------- 3. 拉源码 ----------
info "3/7 拉取源码（$REPO_URL）…"
if [[ -d "$SRC_DIR/.git" ]]; then
  info "目录已存在，更新到最新…"
  git -C "$SRC_DIR" fetch --depth 1 && git -C "$SRC_DIR" reset --hard origin/main
  ok "源码已更新。"
else
  rm -rf "$SRC_DIR"
  git clone --depth 1 "$REPO_URL" "$SRC_DIR"
  ok "源码就绪。"
fi

# ---------- 4. 编译 ----------
info "4/7 编译 screenwall-server（2 核机器约 3–5 分钟，请耐心等待）…"
(cd "$SRC_DIR" && cargo build --release -p screenwall-server)
ok "编译完成。"

# ---------- 5. 安装 + 配置 ----------
info "5/7 安装到 $APP_DIR 并写配置…"
$SUDO mkdir -p "$APP_DIR"
$SUDO cp "$SRC_DIR/target/release/screenwall-server" "$APP_DIR/"
$SUDO chmod 755 "$APP_DIR/screenwall-server"

REG_BOOL=$([ "$REGISTER" = "y" ] && echo true || echo false)
$SUDO tee "$APP_DIR/server.toml" >/dev/null <<EOF
# ScreenWall 服务器配置（由 deploy-server.sh 生成）
bind = "0.0.0.0:$PORT"
db_path = "$APP_DIR/screenwall.db"
allow_registration = $REG_BOOL
token_ttl_days = 30
max_frame_bytes = 1048576
EOF
ok "配置已写入 $APP_DIR/server.toml"

# ---------- 6. 建账号 ----------
info "6/7 创建账号 $ACCOUNT …"
if $SUDO "$APP_DIR/screenwall-server" --config "$APP_DIR/server.toml" user add "$ACCOUNT" --password "$PASSWORD"; then
  ok "账号 $ACCOUNT 已创建。"
else
  warn "账号 $ACCOUNT 可能已存在。如需改密码，运行："
  warn "  $APP_DIR/screenwall-server --config $APP_DIR/server.toml user reset $ACCOUNT"
fi
# 清空密码，避免残留在环境里
unset PASSWORD

# ---------- 7. systemd ----------
info "7/7 安装 systemd 服务并启动…"
$SUDO tee /etc/systemd/system/screenwall.service >/dev/null <<EOF
[Unit]
Description=ScreenWall server
After=network.target

[Service]
Type=simple
ExecStart=$APP_DIR/screenwall-server run --config $APP_DIR/server.toml
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
$SUDO systemctl daemon-reload
$SUDO systemctl enable screenwall
$SUDO systemctl restart screenwall
ok "服务已启动并设为开机自启。"

# ---------- 防火墙 ----------
echo
if command -v ufw >/dev/null 2>&1 && $SUDO ufw status 2>/dev/null | grep -q "Status: active"; then
  $SUDO ufw allow "$PORT"/tcp
  ok "已用 ufw 放行端口 $PORT。"
else
  warn "未检测到活跃的 ufw。请到云控制台【安全组】放行 TCP 端口 $PORT（源 0.0.0.0/0）。"
fi

# ---------- 健康检查 ----------
sleep 1
HEALTH=""
if command -v curl >/dev/null 2>&1; then
  HEALTH=$(curl -fsS "http://127.0.0.1:$PORT/healthz" 2>/dev/null || true)
fi

echo
printf '\n%s\n' "${C_GRN}════════ 部署完成 ════════${C_OFF}"
ok "服务状态：systemctl status screenwall"
if [[ -n "$HEALTH" ]]; then
  ok "健康检查：$HEALTH"
else
  warn "健康检查未通过，用 systemctl status screenwall / journalctl -u screenwall -e 看日志排查。"
fi
echo
echo "客户端连接地址：  http://<服务器公网IP>:$PORT"
echo "  在 ScreenWall 设置 → 连接 → 服务器地址填上面的地址，用 $ACCOUNT 登录。"
echo
warn "当前为明文 HTTP。建议后续加 Caddy 自动 HTTPS（有域名后）。"
info "源码目录 $SRC_DIR 可删除：rm -rf $SRC_DIR"
echo
