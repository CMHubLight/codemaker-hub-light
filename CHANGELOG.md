# 变更记录

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 的结构，版本号遵循语义化版本。

## [未发布]

### 新增

- **代码签名准备**：新增 [代码签名政策](docs/code-signing-policy.md)、SignPath 产物配置模板（`.github/signpath/artifact-configuration.xml`）与 Release 签名工作流（`.github/workflows/release-sign.yml`）。
- 两份 README 增加 Code signing policy 段；`v0.1.0` 安装包仍为未签名，待 SignPath Foundation 审批后由同一工作流重签并替换附件。

## [0.1.0] - 2026-10-27

首个开源版本。

### 新增

- **账号登录**：系统浏览器登录 + 深链回调；浏览器未回跳时可手动粘贴回调链接；回调票据绑定当前会话、一次性消费。
- **凭据管理**：AES-256-GCM 加密落盘（`auth.bin`），主密钥存于系统凭据管理器；支持自动续期、`/validate` 轮询、注销清理。
- **本地代理**：默认 `127.0.0.1:15721`，端口占用时回退 `15722–15800`；支持 Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 与流式返回；剥离客户端鉴权头并注入账号凭据；Anthropic 请求在本地翻译为 OpenAI Chat；仅接受回环 `Host`，拒绝带 `Origin` 的请求。
- **用量统计**：逐请求 token 与费用落库、日聚合、30 天费用、账号额度与模型分布、请求列表筛选与 CSV 导出（UTF-8 BOM、公式前缀中和）。
- **桌面集成**：托盘常驻、关闭窗口即隐藏、单实例、启动失败弹窗与崩溃日志。
- **工程**：CI（前端 / Rust / 文档门禁三 job）、`scripts/check-public-repo.mjs` 脱敏与文档一致性检查。

### 变更（相对开源前的构建）

- **默认网关地址改为占位值 `https://api.example.com`**：首次使用必须在「设置 → 代理与记录」中填写可访问的兼容网关，否则登录与转发都会失败。已存在 `settings.json` 的安装不受影响。
- 移除不随仓库分发的外部材料与旧版功能文档，只保留本项目的代码与文档。
- 文档按当前代码重写：架构、认证、代理、网关契约、用量、设置、数据模型、IPC、开发、排障共十篇。

### 已知限制

- 仅支持 Windows 10/11 x64；无自动更新；macOS / Linux 未验证。
- 原生 Anthropic 上游不受支持，`/v1/messages` 必须走本地翻译，部分 Anthropic 专有字段会被丢弃。
- 上游未返回费用字段时费用记为 0（`cost_source=none`），无本地价格表兜底。
- 请求页无详情弹层，「客户端」列固定为 `unknown`。
- 账号级数字来自上游网关，可能包含其他设备的用量，不能与本机请求明细对账。

[0.1.0]: https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.1.0
