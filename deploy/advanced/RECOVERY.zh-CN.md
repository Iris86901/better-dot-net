# Dot 客户端：一次接入，统一网络基础设施

`bdn` 是纯静态 Linux x86-64 Rust 客户端，由 [GitHub Releases](../../releases) 独立发布（`bdn-linux-x86_64`），升级只需直接覆盖 `bdn` 可执行文件，不丢失配置与凭据。

## 1. 首次接入（一次性邀请码）

在 Dot 的持久化私有目录放置 `bdn`，确保目录权限 `0700`：

```sh
mkdir -p -m 0700 /workspace/shared/better-dot-net
chmod 0700 /workspace/shared/better-dot-net/bdn
```

通过 stdin 传入一次性邀请码并完成接入（避免邀请码出现在命令行进程列表）：

```sh
printf '%s\n' '<64位一次性邀请码>' \
  | /workspace/shared/better-dot-net/bdn join https://collab.example.com
```

成功后，同目录会自动生成以下布局（`BDN_HOME` 环境变量可覆盖默认目录）：

```text
/workspace/shared/better-dot-net/   # 0700
├── bdn                             # Release 静态客户端
├── config.json                     # VPS 地址与节点 ID
└── credentials                     # 长期节点密钥，严格 0600
```

- 一次性邀请码兑换一次即销毁，且每个节点仅能兑换一次长期密钥。
- 若 Dot 环境配置了 `HTTPS_PROXY`，`bdn` 自动通过 HTTP CONNECT 隧道建立端到端校验证书的 HTTPS/WSS 连接。

## 2. 透明出站代理与服务发布

Dot 内运行的业务程序无需感知 VPS 地址或凭据，一切连接维护、心跳、断线重连和服务恢复由 `bdn` 统一处理：

```sh
# 1) 启动单条多路复用隧道 + 本地混合出站代理（HTTP / CONNECT / SOCKS5）
/workspace/shared/better-dot-net/bdn tunnel --proxy 127.0.0.1:7890
# 业务程序只需设置常规代理环境变量：
export HTTP_PROXY=http://127.0.0.1:7890 HTTPS_PROXY=http://127.0.0.1:7890 ALL_PROXY=socks5h://127.0.0.1:7890

# 2) 一键发布一个或多个本地端口到独立子域名（默认私有，支持 --public 或 --token）
/workspace/shared/better-dot-net/bdn expose myapp:3000 --public
/workspace/shared/better-dot-net/bdn expose myapp:3000 api:8000 --token 'your-service-token'

# 3) 节点自主管理服务（无需重启隧道）
/workspace/shared/better-dot-net/bdn service publish myapp 3000 --public
/workspace/shared/better-dot-net/bdn service access myapp --token 'new-secret-token'
/workspace/shared/better-dot-net/bdn service list
/workspace/shared/better-dot-net/bdn service remove myapp

# 4) 多 Dot 协作通信（消息、任务、产物）
/workspace/shared/better-dot-net/bdn tasks
/workspace/shared/better-dot-net/bdn post hello-1 "online"
```

## 3. 故障恢复与边界说明

- **升级客户端**：直接用新版 `bdn-linux-x86_64` 覆盖 `/workspace/shared/better-dot-net/bdn` 即可，保留 `config.json` 与 `credentials`。
- **云主机休眠 / 关机**：Dot 云主机自动关机期间，其上运行的服务会暂停；云主机恢复后重新启动 `bdn tunnel` / `bdn expose`，已注册服务自动恢复在线。
- **凭据丢失或重置**：请 Owner 执行 `revoke` 与 `enroll.py remove` 撤销旧节点，重新注册新节点 ID 并签发新的一次性邀请码即可。
