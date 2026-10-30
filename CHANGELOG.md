# 变更记录

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 的结构，版本号遵循语义化版本。

## [0.1.1] - 2026-10-30

### 新增

- **账号 + AuthKey 直登**：登录页新增「账号 + AuthKey」入口，单次 `POST /api/v2/tokens` 完成登录，不打开浏览器、不轮询；成功后仍走 `/validate`、模型目录、`/get_auth_key` 与额度收尾。AuthKey 只在本机 Rust 侧加密保存，不写日志、不回显前端。
- **请求明细「首字节」列**：请求页展示流式响应的首字节延迟（`first_byte_ms`），便于区分网关排队与本地耗时。
- **手动「重新检测 / 续期」**：设置 → 账户可强制用 `auth_key` 续期一次并补一次 `/validate`，成功后刷新账号档案与模型目录；失败原因显示在账户面板。
- **退出登录二次确认**：避免误触清除本地凭据。

### 变更

- **过期自动续期**：`/validate` 遇到 401/403 时先用 `auth_key` 自动续期并重试一次；续期不可用或仍被拒才进入「需要重新登录」。网络瞬时失败仍回落离线宽限，不误判失效。
- **首字节口径**：只统计上游响应第一个数据块到达的时间；非流式请求与无响应体的请求不产生该值。
- IPC 命令 19 → 20（新增 `auth_login_with_auth_key`）。

### 修复

- **崩溃现场可诊断性**：`panic.log` 改为**先同步落盘最小现场**（时间、线程、位置、消息）再尝试回溯与 `tracing`。此前若日志订阅者或回溯符号化失败，`panic = "abort"` 的进程可能"崩了却什么都没写"。
- **应用无声消失可区分**：启动时写 `logs\session.marker`，正常退出时先追加 `logs\exit.log` 再删除 marker；下次启动发现上次未正常退出会在主日志记一条 WARN，用于区分"崩溃 / 被外部终止"与"正常退出"（例如被任务管理器结束或 `taskkill` 强杀）。
- 同一 tick 内可能重复续期：`renew_if_due` 改为读取最新凭据快照，避免同一过期窗口内发两次 `/renew_token`。
- 登录成功后并发收尾时，AuthKey 路径会确认新 token 真的写入内存，避免把「被单飞跳过」误报为登录成功。

### 工程

- **构建防呆**：`pnpm tauri build` 在 `beforeBuildCommand` 先检查本仓库 `src-tauri/target/release/` 是否被运行中的实例或安装包占用，命中时立即失败并提示用托盘「退出」正常关闭（**不会自动结束进程**），避免再出现"为解锁 exe 强杀实例"造成的假崩溃现场。路径判定与提示文案有 `node:test` 单测（`pnpm test:scripts`，CI frontend job 执行）。
- 排障文档补充进程审计取证 runbook：开启后下一次事故可定位"是谁终止了进程"（默认关闭，需管理员，见 `docs/troubleshooting.md` §4.5）。
- CI 升级到 Node 24 兼容的 action 版本（checkout / setup-node / upload-artifact v7，pnpm/action-setup v6），消除 Node 20 弃用与 `ubuntu-latest` 迁移告警。
- **代码签名准备**：新增 [代码签名政策](docs/code-signing-policy.md)、SignPath 产物配置模板（`.github/signpath/artifact-configuration.xml`）与 Release 签名工作流（`.github/workflows/release-sign.yml`）；`v0.1.1` 安装包仍为未签名，待 SignPath Foundation 审批后由同一工作流重签并替换附件。
- 发布流程补全：`docs/development.md` 增加发布前烟雾测试与源码包/校验和生成步骤。

### 已知限制

- 仅支持 Windows 10/11 x64；无自动更新；macOS / Linux 未验证。
- 原生 Anthropic 上游不受支持，`/v1/messages` 必须走本地翻译，部分 Anthropic 专有字段会被丢弃。
- 上游未返回费用字段时费用记为 0（`cost_source=none`），无本地价格表兜底。
- 请求页无详情弹层，「客户端」列固定为 `unknown`。
- 账号级数字来自上游网关，可能包含其他设备的用量，不能与本机请求明细对账。
- AuthKey 直登要求兼容网关实现 `POST /api/v2/tokens`；未实现的网关可在登录页改用浏览器登录。

[0.1.1]: https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.1.1
[0.1.0]: https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.1.0
