# 参与 Better Dot Net 开发

[English](CONTRIBUTING.md)

欢迎通过 bug 报告、文档改进、focused fix，以及符合 Better Dot Net 架构方向（低内存 Rust 核心、基础版 / 进阶版 / 客户端解耦）的新能力参与贡献。

也欢迎使用 coding agent 辅助完成贡献。无论使用什么工具，提交者仍需对最终 diff 负责，完成必要验证，并确认不包含任何凭据、个人域名、公网 VPS IP 或机器私有数据。

## 开始之前

- 开始较大改动前，请先查看最新 `main` 与已有 issues。
- 小型文档修正与明确的 bug 修复可直接提交 PR；较大的功能、协议改动或架构调整建议先提 issue 对齐方向，避免重复工作。
- 保持基础版服务端（`better-dot-net`）、进阶版服务端（`bdn-collab` / `bdn-terminal`）与静态客户端（`bdn`）相互解耦；升级任一模块都必须保留已有凭据与通信能力。
- 保持改动集中（focused），不要在同一个 PR 中混入无关重构、格式噪音或临时生成物。
- 严禁在 issue、PR、日志、代码或截图中提交真实 Access Key、邀请码、Bearer Token、Cookie、私钥、个人域名、公网 IP 或未清洗 EXIF 的图片。

## 提交 Issue

一个高质量的 issue 应尽量让维护者无需追问基础上下文就能定位问题；只填写与问题相关的字段，不必机械填写 `N/A`。

提交 bug 或站点/环境兼容性问题时，建议包含：

- **实际现象与影响：** 涉及哪个模块（基础版 / 进阶版 / `bdn`）、具体操作或目标网站、实际结果以及是否阻塞主流程。
- **运行环境：** Better Dot Net 版本标签或 commit、VPS 系统与架构、反代类型（`Caddy` / `Nginx`）、浏览器或 Agent 平台（`Dot`、`Muse`、`Hark`、本地 Chrome 等）。
- **最小复现步骤：** 触发问题的最短步骤，并注明必现还是偶发。
- **预期行为与实际行为：** 分开写清。
- **原始证据：** 保留准确的错误文本、HTTP 状态码、控制台报错或脱敏后的日志尾部，避免只写转述。
- **事实与推断分开：** 明确区分直接观察到的现象与推测的根因。

### 由 Coding Agent 创建的 Issue

欢迎 coding agent 提交 issue，提交前应优先附上低成本即可获取的诊断信息（原始报错、检查过的 commit 与文件、针对性测试证据），并遵守：

- 明确标明问题是**直接复现**、**基于源码/测试推断**，还是**仅转述用户反馈未独立复现**；
- 禁止编造缺失的环境信息、复现结果或根因；
- 尚未被证据证实的根因必须标注为 **suspected（疑似）**。

## 开发与自测流程

1. 基于最新 `main` 创建单一目标分支（如 `fix/...`、`feat/...`、`docs/...`）。
2. 遵循现有架构与命名习惯，优先做最小、完整、清晰的改动。
3. 行为改动同步补充或更新针对性测试；公开用法或部署流程变化时，同步更新对应的中/英文文档。
4. 提交 PR 前按改动范围执行最小必要验证：
   - **网页代理前端（`web/`）：**
     ```bash
     node --check web/app.js && node --check web/sw.js
     node scripts/verify-reconnect.mjs
     ```
   - **Rust 基础版（`better-dot-net`）：**
     ```bash
     cargo fmt --check
     cargo test --locked --bin better-dot-net
     ```
   - **Rust 进阶版 / 客户端（`bdn-collab` / `bdn` / `bdn-terminal`）：**
     ```bash
     cargo fmt --check
     cargo test --locked --features advanced,collab-cli,terminal-cli
     ```
   - **纯文档改动：** 运行 `git diff --check`，核对链接、锚点与命令；无需运行 Cargo 构建。
5. 提交前逐项检查 `git status` 与 `git diff`，确认没有无关修改、临时文件、凭据或个人域名/IP。

## Pull Request 规范

每个 PR 应保持可评审，且只解决**一个完整而集中的目标**。PR 描述请说明：

- **改动内容与原因**（若关联 issue 请注明 `Fixes #<id>` / `Refs #<id>`）。
- **验证方式与结果**（执行的具体测试命令与结果，或实际浏览器 / VPS 验证步骤）。
- **信任边界回归证据：** 若涉及认证、Cookie 隔离、SSRF/DNS 拦截、Clash YAML 清洗、隧道与公网服务访问控制、节点登记/撤销或持久化，请提供对应的回归测试或验证证据。
- **隐私与清洁度确认：** 确认代码、文档与静态资源中均不含真实密钥、个人域名/IP 及隐私元数据。
