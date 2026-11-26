# Better Dot Net：基础版部署指南

[English](README.md) · **简体中文**

基础版打包了 Rust 代理网关（`better-dot-net`）、浏览器运行时（`web/`）以及锁版本的 `meow-rs` 多协议核心。即使默认使用 VPS 直连，`meow-rs` 核心也会在环回端口就绪，方便随时在网页面板零中断切换到上游节点。基础版与进阶版完全独立。

> **建议**：如果后续有升级进阶版的计划，基础版推荐直接使用 **Caddy** 作为 HTTPS 入口，后续接入进阶版和泛域名分发时无需更换反代架构。一台低配甲骨文 AMD 小鸡（如 1 GiB 内存）即可流畅运行。

---

## 1. 获取并校验发布包

直接从 [GitHub Releases](../../releases) 下载 `basic-linux-x86_64.tgz` 与 `basic-linux-x86_64.tgz.sha256`（或按需自行编译并通过 `bash deploy/basic/package.sh <better-dot-net二进制> <新输出目录> <meow压缩包>` 打包）。

在目标 Linux x86-64（glibc >= 2.34）VPS 上执行校验与解压：

```sh
sha256sum -c basic-linux-x86_64.tgz.sha256
sudo mkdir -p /opt/better-dot-net-basic/releases/v0.2.0
sudo tar -xzf basic-linux-x86_64.tgz -C /opt/better-dot-net-basic/releases/v0.2.0 --strip-components=1
(cd /opt/better-dot-net-basic/releases/v0.2.0 && sha256sum -c SHA256SUMS)
sudo ln -sfn /opt/better-dot-net-basic/releases/v0.2.0 /opt/better-dot-net-basic/current
```

确保 `/opt/better-dot-net-basic` 及其下 `web/` 目录对 Caddy 可读（目录 `0755`、静态文件 `0644`），二进制保持 root 所有。

---

## 2. 创建系统用户与环境变量

```sh
sudo install -D -m 0644 /opt/better-dot-net-basic/current/better-dot-net-basic.sysusers \
  /etc/sysusers.d/better-dot-net-basic.conf
sudo systemd-sysusers /etc/sysusers.d/better-dot-net-basic.conf

sudo mkdir -p -m 0750 /etc/better-dot-net-basic
ACCESS_KEY=$(openssl rand -base64 18 | tr '+/' '-_' | tr -d '=')
COOKIE_SECRET=$(openssl rand -base64 24 | tr '+/' '-_' | tr -d '=')
MEOW_SECRET=$(openssl rand -base64 24 | tr '+/' '-_' | tr -d '=')

sudo tee /etc/better-dot-net-basic/environment >/dev/null <<EOF
HOST=127.0.0.1
PORT=3030
ACCESS_KEY=${ACCESS_KEY}
COOKIE_SECRET=${COOKIE_SECRET}
MEOW_SECRET=${MEOW_SECRET}
EOF
sudo chown root:root /etc/better-dot-net-basic/environment
sudo chmod 0600 /etc/better-dot-net-basic/environment
echo "ACCESS_KEY=${ACCESS_KEY}"
```

- 请妥善保管输出的 `ACCESS_KEY`，不要写入公开 URL、聊天记录或 Git。
- `meow-rs` 的控制端口（默认 `19190`）和本地代理端口（默认 `17990`）均严格绑定环回地址 `127.0.0.1`。

---

## 3. 安装 systemd 服务与 Caddy 入口

```sh
sudo install -m 0644 /opt/better-dot-net-basic/current/better-dot-net-basic-core.service \
  /etc/systemd/system/better-dot-net-basic-core.service
sudo install -m 0644 /opt/better-dot-net-basic/current/better-dot-net-basic.service \
  /etc/systemd/system/better-dot-net-basic.service
sudo systemctl daemon-reload
sudo systemctl enable --now better-dot-net-basic.service
```

将包内 `Caddyfile` 站点块合并到你的 `/etc/caddy/Caddyfile`（把 `proxy.example.com` 改为你的域名或已配好证书的 HTTPS IP），校验并重启 Caddy：

```sh
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl restart caddy.service
```

---

## 4. 部署验证

```sh
curl -sf http://127.0.0.1:3030/health
sudo systemctl is-active better-dot-net-basic-core.service better-dot-net-basic.service caddy.service
```

在浏览器打开 `https://proxy.example.com/`，输入 `ACCESS_KEY` 解锁后确认右上角状态为 `Ready`，并打开一个测试网页验证正常加载与刷新恢复。

---

## 5. 网页面板与上游路由

首页 Egress Routing 面板提供三档出口：

- **Direct VPS**：直接从 VPS 出站。
- **Pasted Nodes**：手动粘贴单行或多行代理节点（支持 `host:port`、`host:port:user:password`、`http://`、`socks5://`、`vless://`、`vmess://`、`trojan://`、`ss://`、`hysteria2://`、`anytls://` 以及 Clash YAML）。
- **Remote Clash**：默认支持已登录用户输入公网 HTTPS（443 端口）Clash 订阅链接一键导入（服务端自动校验 TLS 证书、禁用重定向、拦截私网/环回/云元数据 SSRF，并按白名单重序列化 `proxies:`；原始订阅 URL 与参数从不落盘或回显）。管理员可通过 `BDN_SUBSCRIPTION_HOSTS=host1,host2` 限制允许的订阅域名，或设为 `none` 关闭远程订阅导入。

切换节点只影响新建立的连接，已有暖流和浏览器本地 Cookie 不受影响。

---

## 6. 裸 IP HTTPS 访问说明 {#access-by-literal-ip}

不配置域名时，也可通过 `https://<公网IP>[:端口]/` 访问基础版（实测 Hark 与 Muse 支持，**Dot 实测会遇到证书错误，Dot 请使用域名**）：

- 边缘网关（Caddy）必须配置包含该 IP 的 `subjectAltName` 且受浏览器信任的 TLS 证书，并在无 SNI 的 IP 连接上正确下发该证书。
- 公网明文 HTTP 无法提供 Service Worker 所需的 Secure Context，不能用于公网部署。
- 裸 IP 与域名在浏览器中属于不同的 Origin，拥有各自独立的登录态和本地 Cookie。

---

## 7. 升级与回滚

将新版压缩包解压到新的版本目录（例如 `/opt/better-dot-net-basic/releases/v0.2.1`），校验 `SHA256SUMS` 后原子切换 `current` 软链并重启两个基础版服务即可（保留 `/etc/better-dot-net-basic/environment` 与状态目录）：

```sh
sudo ln -sfn /opt/better-dot-net-basic/releases/v0.2.1 /opt/better-dot-net-basic/current
sudo systemctl restart better-dot-net-basic-core.service better-dot-net-basic.service
```

回滚时将 `current` 指回旧目录并执行同样的重启命令，不影响进阶版和其他站点。
