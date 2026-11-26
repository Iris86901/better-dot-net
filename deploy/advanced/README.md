# 进阶版服务端部署：一台 VPS、多台 Dot

进阶版与基础版网页代理完全解耦，使用独立进程、独立 HTTPS 域名、独立凭据目录和独立状态目录。只面向 Dot（支持多台 Dot 独立接入与管理）。

## 1. 获取并安装进阶版服务端（VPS 执行一次）

1. 从 [GitHub Releases](../../releases) 下载 `advanced-server-linux-x86_64.tgz` 与 `.sha256`（也可自行用 `cargo build --release --locked --features advanced,terminal-cli --bin bdn-collab --bin bdn-terminal` 构建并用 `bash deploy/advanced/package.sh target/release dist` 打包）。
2. 校验 `sha256sum -c advanced-server-linux-x86_64.tgz.sha256` 与解压后的 `SHA256SUMS`，安装到 `/opt/better-dot-net-advanced/`（同时可从 Release 下载静态 `bdn-linux-x86_64` 放到 `/opt/better-dot-net-advanced/bin/bdn` 供 Owner 本地管理）。
3. 创建固定系统用户 `bdn-collab`：
   ```sh
   install -D -m 0644 bdn-collab.sysusers /etc/sysusers.d/bdn-collab.conf
   systemd-sysusers /etc/sysusers.d/bdn-collab.conf
   ```
4. 初始化 `/etc/better-dot-net-advanced/nodes.json`（`0600`）与 `owner.token`，详见[节点管理](ENROLLMENT.md)。
5. 安装 `bdn-collab.service`（监听 `127.0.0.1:9300`），并在 drop-in（如 `/etc/systemd/system/bdn-collab.service.d/40-agent-invite.conf`）配置所需功能：
   ```ini
   [Service]
   Environment=BDN_INVITE_DIR=/var/lib/better-dot-net-advanced/invites
   # 可选：若需要浏览器邀请页再配置精确的 HTTPS Origin
   Environment=BDN_RECOVERY_ORIGIN=https://collab.example.com
   # 可选：开启独立子域名反向服务发布
   Environment=BDN_EXPOSE_DOMAIN=svc.example.com
   # 可选：开启 Dot 经 VPS 的透明出站代理（VPS 侧自动拦截私网/环回/云元数据 SSRF）
   Environment=BDN_EGRESS_ENABLED=1
   ```
6. 合并 [Caddyfile](Caddyfile)（控制面包含 `/v1/action`、`/v1/artifact/*`、`/v1/tunnel` 与 `/agent/invite`；如需对外发布子域名服务，再预配 `*.svc.example.com` 泛域名 DNS 与泛域名 TLS 证书并反代至 `127.0.0.1:9300`）。校验配置后重启 `bdn-collab.service` 和 `caddy.service`。

## 2. 为新 Dot 节点签发一次性邀请码

```sh
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py add dot-1
sudo systemctl restart bdn-collab.service
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 /etc/better-dot-net-advanced/owner.token grant dot-1
sudo python3 /opt/better-dot-net-advanced/sbin/invite.py dot-1 --minutes 720
```

仅通过私密渠道将**认证地址**（`https://collab.example.com`）与输出的一次性邀请码交给对应的 Dot 节点即可。

## 3. Owner 全局管理与应急控制

```sh
# 查看全集群已发布服务
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 /etc/better-dot-net-advanced/owner.token service list

# 立即下线任意节点发布的服务
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 /etc/better-dot-net-advanced/owner.token service remove <subdomain>

# 撤销节点权限（立即断开该节点活跃隧道并移除其所有已发布服务）
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 /etc/better-dot-net-advanced/owner.token revoke dot-1
```

客户端接入与日常使用见 [Dot 接入与使用](RECOVERY.zh-CN.md)。
