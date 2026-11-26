# 进阶版节点身份管理（`enroll.py` / `invite.py`）

[English](ENROLLMENT.md) · **简体中文**

进阶版服务端 `bdn-collab` 采用两层分离校验：

1. **认证目录（`nodes.json`）**：保存 `节点 ID -> 凭据 SHA-256 摘要` 映射，**在服务启动/重启时加载**；凭证未通过校验返回 `401`。
2. **项目成员权限（状态目录）**：记录已授权的节点 ID，Owner 执行 `grant` / `revoke` 时**实时生效**；已认证但未授权或已撤销的节点返回 `403`。

仅在 `nodes.json` 添加节点摘要不会自动赋予访问权限；`owner` 身份不能通过本工具删除。

---

## 1. 新增节点并签发一次性邀请码

在 VPS 上以凭据目录属主（参考部署中为 `root`）执行，不要在用户可写目录直接用 `sudo` 运行脚本：

```sh
# 1) 查看现有节点
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py list

# 2) 注册新节点（仅把 SHA-256 写入 nodes.json，原始密钥以 0600 落盘且永不打印）
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py add dot-1

# 3) 重启服务加载新的 nodes.json
sudo systemctl restart bdn-collab.service

# 4) 由 Owner 授予该节点项目访问权限
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 \
  /etc/better-dot-net-advanced/owner.token grant dot-1

# 5) 签发一次性邀请码（默认有效 720 分钟，兑换一次即销毁）
sudo python3 /opt/better-dot-net-advanced/sbin/invite.py dot-1 --minutes 720
```

仅通过私密渠道把**认证地址**与输出的一次性邀请码交给对应 Dot，切勿把长期密钥明文发到聊天记录、URL 或 Git。

---

## 2. 撤销权限与节点退役

若节点密钥泄露或需下线某台 Dot，先执行 `revoke`（**立即生效**：立刻断开该节点的活跃 WSS 隧道并移除其发布的全部子域名服务），再从认证目录中移除摘要并重启服务：

```sh
sudo /opt/better-dot-net-advanced/bin/bdn 127.0.0.1:9300 \
  /etc/better-dot-net-advanced/owner.token revoke dot-1
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py remove dot-1
sudo rm -f /etc/better-dot-net-advanced/node-dot-1.token
sudo systemctl restart bdn-collab.service
sudo python3 /opt/better-dot-net-advanced/sbin/enroll.py list
```

- 被移除的节点 ID 会写入永久退役标记（`.bdn-retired-<id>`），防止同名 ID 带着旧成员记录被误复用；如需重新接入，请注册新的节点 ID（例如 `dot-2`）。
- 并发管理请始终使用 `enroll.py`（内置文件锁与 `O_NOFOLLOW` 权限校验），不要手工编辑 `nodes.json`。

---

## 3. 本地回归验证

```sh
python3 scripts/verify-enrollment.py
python3 scripts/verify-enrollment.py target/release/bdn-collab
```
