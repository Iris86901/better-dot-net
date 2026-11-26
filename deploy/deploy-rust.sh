#!/bin/bash
set -euo pipefail

APP_DIR="/opt/dot-proxy"
ENV_DIR="/etc/dot-proxy"
STATE_DIR="/var/lib/dot-proxy"
BUNDLE="/tmp/dot-proxy-bundle.tar.gz"

MEOW_VER="v0.29.0"
MEOW_URL="https://github.com/meow-rs/meow-rs/releases/download/${MEOW_VER}/meow-${MEOW_VER}-x86_64-unknown-linux-musl.tar.gz"
MEOW_SHA256="92fe902cca5e95799af02a42fcda9a4f9195aa61ec126057b51b15a8d2116ca2"

echo "[1/5] Ensuring dotproxy user & least-privilege directories..."
id -u dotproxy >/dev/null 2>&1 || useradd --system --no-create-home --shell /sbin/nologin dotproxy
mkdir -p "$APP_DIR" "$ENV_DIR" "$STATE_DIR"

echo "[2/5] Extracting Rust gateway + web assets..."
# Preserve existing meow binary if already installed at expected version
MEOW_SAVED=""
if [ -x "${APP_DIR}/bin/meow" ]; then
  if "${APP_DIR}/bin/meow" --version 2>/dev/null | grep -q "${MEOW_VER#v}"; then
    cp -p "${APP_DIR}/bin/meow" /tmp/meow-bin-keep
    MEOW_SAVED="1"
  fi
fi

rm -rf "${APP_DIR:?}/bin" "${APP_DIR:?}/web" "${APP_DIR:?}/deploy"
tar -xzf "$BUNDLE" -C "$APP_DIR"
rm -f "$BUNDLE"

if [ -n "$MEOW_SAVED" ]; then
  mv /tmp/meow-bin-keep "${APP_DIR}/bin/meow"
else
  echo "[2.5/5] Installing meow-rs ${MEOW_VER} static musl binary..."
  MEOW_TMP=$(mktemp -d)
  curl -fsSL "$MEOW_URL" -o "${MEOW_TMP}/meow.tar.gz"
  ACTUAL_SHA=$(sha256sum "${MEOW_TMP}/meow.tar.gz" | awk '{print $1}')
  if [ "$ACTUAL_SHA" != "$MEOW_SHA256" ]; then
    echo "ERROR: meow-rs SHA256 mismatch: expected=$MEOW_SHA256 actual=$ACTUAL_SHA" >&2
    rm -rf "$MEOW_TMP"
    exit 1
  fi
  tar -xzf "${MEOW_TMP}/meow.tar.gz" -C "$MEOW_TMP"
  cp "${MEOW_TMP}/meow-${MEOW_VER}-x86_64-unknown-linux-musl/meow" "${APP_DIR}/bin/meow"
  rm -rf "$MEOW_TMP"
fi

# Runtime assets: readable by dotproxy & caddy (for static serving), writable only by root.
chown -R root:dotproxy "$APP_DIR"
find "$APP_DIR" -type d -exec chmod 0755 {} +
find "$APP_DIR" -type f -exec chmod 0644 {} +
chmod 0750 "${APP_DIR}/bin/dot-proxy" "${APP_DIR}/bin/meow"

# Secrets / environment: strictly readable ONLY by root and dotproxy (never world-readable).
if [ ! -f "${ENV_DIR}/env" ]; then
  KEY="${ACCESS_KEY_INIT:-}"
  if [ -z "$KEY" ]; then
    KEY=$(head -c 18 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=')
  fi
  cat > "${ENV_DIR}/env" <<EOF
HOST=127.0.0.1
PORT=3030
ACCESS_KEY=${KEY}
EOF
fi

if ! grep -q '^MEOW_SECRET=' "${ENV_DIR}/env"; then
  MSEC=$(head -c 24 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=')
  cat >> "${ENV_DIR}/env" <<EOF
STATE_DIR=/var/lib/dot-proxy
MEOW_BIN=/opt/dot-proxy/bin/meow
MEOW_SOCKS=127.0.0.1:17890
MEOW_API=127.0.0.1:19090
MEOW_SECRET=${MSEC}
EOF
fi

chown root:dotproxy "$ENV_DIR" "${ENV_DIR}/env"
chmod 0750 "$ENV_DIR"
chmod 0640 "${ENV_DIR}/env"

MEOW_SECRET_VAL=$(grep '^MEOW_SECRET=' "${ENV_DIR}/env" | head -n1 | cut -d= -f2- | tr -d '\r')

# Private runtime state dir for meow.yaml & routing.json (0700 dotproxy:dotproxy)
chown -R dotproxy:dotproxy "$STATE_DIR"
chmod 0700 "$STATE_DIR"

if [ ! -f "${STATE_DIR}/meow.yaml" ]; then
  cat > "${STATE_DIR}/meow.yaml" <<EOF
mixed-port: 17890
allow-lan: false
bind-address: "127.0.0.1"
mode: rule
log-level: warning
external-controller: "127.0.0.1:19090"
secret: "${MEOW_SECRET_VAL}"

proxies: []

proxy-groups:
  - name: "EGRESS"
    type: select
    proxies:
      - DIRECT

rules:
  - IP-CIDR,127.0.0.0/8,REJECT,no-resolve
  - IP-CIDR,10.0.0.0/8,REJECT,no-resolve
  - IP-CIDR,172.16.0.0/12,REJECT,no-resolve
  - IP-CIDR,192.168.0.0/16,REJECT,no-resolve
  - IP-CIDR,169.254.0.0/16,REJECT,no-resolve
  - IP-CIDR6,::1/128,REJECT,no-resolve
  - IP-CIDR6,fc00::/7,REJECT,no-resolve
  - IP-CIDR6,fe80::/10,REJECT,no-resolve
  - MATCH,EGRESS
EOF
  chown dotproxy:dotproxy "${STATE_DIR}/meow.yaml"
  chmod 0600 "${STATE_DIR}/meow.yaml"
fi

echo "[3/5] Updating Caddyfile & hardened systemd units..."
DOMAIN="${DOT_PROXY_DOMAIN:-}"
if [ -z "$DOMAIN" ] && [ -f /etc/caddy/Caddyfile ]; then
  DOMAIN=$(grep -Eo 'https://[^:[:space:]]+:8443' /etc/caddy/Caddyfile | head -n1 | sed 's|^https://||; s|:8443$||' || true)
fi
DOMAIN="${DOMAIN:-proxy.example.com}"
install -m 0644 -o root -g root "${APP_DIR}/deploy/Caddyfile" /etc/caddy/Caddyfile
sed -i "s|proxy\.example\.com|${DOMAIN}|g" /etc/caddy/Caddyfile
install -m 0644 -o root -g root "${APP_DIR}/deploy/dot-proxy-core.service" /etc/systemd/system/dot-proxy-core.service
install -m 0644 -o root -g root "${APP_DIR}/deploy/dot-proxy.service" /etc/systemd/system/dot-proxy.service
sed -i 's/\r$//' /etc/caddy/Caddyfile /etc/systemd/system/dot-proxy-core.service /etc/systemd/system/dot-proxy.service
chmod 0755 /etc/caddy /etc/nginx/stream.d 2>/dev/null || true
chmod 0644 /etc/caddy/Caddyfile
[ -f /etc/nginx/stream.d/sni.conf ] && chmod 0644 /etc/nginx/stream.d/sni.conf

if [ -f /tmp/dot-proxy-src.tar.gz ] && [ -n "${SYNC_SRC_DIR:-}" ]; then
  echo "[4/5] Syncing source snapshot to ${SYNC_SRC_DIR}..."
  mkdir -p "$SYNC_SRC_DIR"
  if [ -f "${SYNC_SRC_DIR}/NOTES.local.md" ]; then
    cp -p "${SYNC_SRC_DIR}/NOTES.local.md" /tmp/notes-local-keep.md
  fi
  rm -rf "${SYNC_SRC_DIR:?}/.git" "${SYNC_SRC_DIR:?}"/*
  tar -xzf /tmp/dot-proxy-src.tar.gz -C "$SYNC_SRC_DIR"
  rm -f /tmp/dot-proxy-src.tar.gz
  if [ -f /tmp/notes-local-keep.md ]; then
    mv /tmp/notes-local-keep.md "${SYNC_SRC_DIR}/NOTES.local.md"
  fi
  if [ -n "${SYNC_SRC_OWNER:-}" ]; then
    chown -R "$SYNC_SRC_OWNER" "$SYNC_SRC_DIR"
  fi
  find "$SYNC_SRC_DIR" -type d -exec chmod 0750 {} +
  find "$SYNC_SRC_DIR" -type f -exec chmod 0640 {} +
  chmod 0750 "${SYNC_SRC_DIR}/deploy/"*.sh 2>/dev/null || true
fi

/usr/local/bin/caddy validate --config /etc/caddy/Caddyfile >/dev/null
nginx -t

echo "[5/5] Starting meow-rs core, dot-proxy gateway, and restarting Caddy..."
systemctl daemon-reload
systemctl enable --now dot-proxy-core.service dot-proxy.service
systemctl restart dot-proxy-core.service
sleep 1
systemctl restart dot-proxy.service caddy.service

sleep 1
echo "=== STATUS ==="
for s in ${EXTRA_CHECK_SERVICES:-} dot-proxy-core dot-proxy caddy nginx; do
  printf "%-24s %s\n" "$s" "$(systemctl is-active ${s}.service)"
done
echo "=== PERMISSIONS ==="
ls -ld /opt/dot-proxy /opt/dot-proxy/bin/dot-proxy /opt/dot-proxy/bin/meow /etc/dot-proxy /etc/dot-proxy/env /var/lib/dot-proxy /var/lib/dot-proxy/meow.yaml
free -h
ps -eo pid,user,rss,comm --sort=-rss | grep -E 'dot-proxy|meow|caddy|nginx' || true
