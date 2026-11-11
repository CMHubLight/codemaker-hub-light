# 变更记录

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 的结构，版本号遵循语义化版本。

## [Unreleased]

### 修复

- **旧轮询会覆盖后完成的登录**：`auth_poll_status` 拿到 tokens 后只校验 epoch 与 `login_serial`，而 `start_login` / `commit_login` 都不推进 epoch，因此"旧轮询早发出、晚返回"能覆盖用户刚用 AuthKey 完成的登录（内存、档案与 `auth.bin` 一起换人），反向的深链路径则被 `poll_uuid` 绑定挡住。现在轮询/深链的收尾必须绑定发起时的 `pending.poll_uuid`（`LoginBinding`），入口、写盘前、提交时各校验一次；被取代的收尾静默作废，且**不删**可能已属于新会话的 `auth.bin`。等待态下 UI 同时禁用 AuthKey 提交并提示先取消等待。
- **登录/校验返回的 `serverUrl` 绕过"http 仅回环"校验**：代理原来只判 `http(s)://` 前缀，且该地址优先级高于设置里校验过的网关，明文 http 可把 `x-access-token` / `Authorization` 发往任意主机（含带凭据的盲 SSRF）。现在 `serverUrl` 与设置项走同一套 `gateway_url_is_acceptable` 策略，不合法时在凭据快照处清空、并在代理转发处回退设置网关 + warn。
- **透传路由压缩响应导致 usage/cost 静默记 0**：`accept-encoding` 原样转发给上游，而 reqwest 未启用解压特性，上游压缩时客户端正常、代理却解析不出 usage。现在所有转发路由（不只翻译路由）统一剥离客户端 `accept-encoding` 并注入 `identity`；上游仍返回压缩体时记一条 warn 便于定位。

### 安全

- **签名 token 不再挂 job 级 env**：`SIGNPATH_*` 只出现在"检查配置"step 的 `env:` 与"提交签名请求"step 的 `with:`，避免被同 job 的 `pnpm install` / `pnpm tauri build` 及其依赖构建脚本读取。
- **第三方 Action 全部固定到 commit SHA**（含 `dtolnay/rust-toolchain`），CI 增加 workflow 级 `permissions: contents: read`，两个工作流的 checkout 均 `persist-credentials: false`。
- **公开仓库改中性措辞并外置脱敏词表**：`.gitignore` 与门禁脚本不再描述本地材料的性质，脚本内嵌的厂商/个人/内部标识改为"形状类"通用规则；具体词表由维护者通过 `REDACTION_TERMS_FILE` 在本地提供，不再随仓库公开。

## [0.2.1] - 2026-11-11

### 修复

- **下游停读导致流式请求永久挂住**：响应生产者原来的两处 `tx.send().await` 写在 `select!` 之外。客户端停读（TCP 零窗口）把 16 槽通道塞满后，写出永久 Pending，300 s 上游空闲超时因 await 点不在 `select!` 内而完全失效——任务不结束、上游连接与内存不释放、该请求也永不落库。现在两处写出统一走 `send_with_idle_deadline`，与上游读取共享同一个 deadline，失败按 `client_aborted` / `timeout` 落库。
- **上游中途断流被伪装成完整回复**：`/v1/messages` 翻译路由在上游中途断开或空闲超时后会补 `content_block_stop` + `message_delta`（usage 全 0）+ `message_stop`，客户端看到的是"成功且完整"的回复，实际是被截断的答案，并按 0 token 记账。现在 `StreamTranslator::finish(failed)` 区分结束原因：失败路径关闭已开内容块后发 `event: error`，不再发 `message_delta` / `message_stop`；"上游一帧都没有"的空流语义保持不变。
- **已登录时一次认证状态刷新失败会打回失败屏**：`app://second-instance` 或深链回调触发的 `auth_status` 失败（含 10 s 超时）会把内存里的有效登录态覆盖成 error 占位并置 `bootFailed`，直接切到全屏"无法连接本地后端"，页面内的筛选与未保存编辑随之丢失。现在只有"尚没有任何有效登录"时才进入失败屏，其余情况显示内联"登录状态异常"并保留当前界面。
- **请求页翻页失败后页码与内容错配**：`useAsyncResource` 按设计在失败时保留旧数据，页面又把旧页的 `next_cursor` 当作兜底继续前进，于是出现"第 3 页显示第 2 页内容"和重复游标 `[null, c1, c1]`。资源返回值现在携带所属查询 `key`，只有当前页的响应才参与渲染与前进；失败页显示错误与重试，不再复用上一页行。
- **浅色主题对比度不达 WCAG AA**：`--text-muted` 在 `--bg` / `--bg-subtle` 上仅 2.79:1 / 2.66:1，`--success` 在 `--success-soft` 上 4.37:1，`.pill--info` 也不足 4.5:1。分别调整为 5.0:1 / 5.4:1，并新增对比度回归测试防止回退。

### 安全

- **rustls-webpki 0.103.15 → 0.103.16**（RUSTSEC-2026-0382 / CVE-2026-111943）：证书校验可能接受已吊销的中间 CA。该 crate 由 `reqwest → hyper-rustls → rustls` 引入，正是发布产物里校验网关证书的那一层。
- **rusqlite 0.32 → 0.41、r2d2_sqlite 0.25 → 0.36**，带入 `libsqlite3-sys 0.39.1`（SQLite 3.54.1），修掉 RUSTSEC-2026-0459 / CVE-2026-119375（构造的 `-wal` 可让普通查询返回进程内存残留）。无代码改动。
- `cargo audit` 结果为 0 漏洞（剩余 6 条均为 glib / zbus / proc-macro-error 等 Linux-only 或构建期依赖的 informational 告警）。

### 工程

- 新增回归测试：`send_with_idle_deadline_times_out_when_receiver_stalls`、`broken_upstream_stream_emits_error_without_message_stop`（替换原先"钉住旧行为"的用例）、`src/App.test.tsx`、请求页失败翻页用例、浅色主题对比度用例。
- `docs/proxy.md` 的翻译语义与排障表同步到"失败收尾发 `event: error`、写出同样受 idle 超时约束"。

## [0.2.0] - 2026-11-06

### 移除

- **移除「导出 CSV」功能**：请求页不再提供导出入口，后端 `usage_export_csv` 命令与整条导出链路一并删除（`usage::queries::export_csv`、`db::repo::export_rows_stream`、`db::rows::UsageExportRow`、`db::sql::usage_export`、`state::exports_dir`，以及前端 `UsageExportResult` / `CommandName` 成员 / mock 分支）。IPC 命令数 20 → 19；数据库结构与设置项不变，历史已导出的 `exports/*.csv` **不会被删除**，需要时请手动清理该目录。

### 修复

- **注销后本地凭据可能复活（安全）**：`store.delete` 现在**先删系统凭据库主密钥、再删 `auth.bin`**，两侧都尝试、不短路；`logout` 失败会重试一次，仍失败则写 `error!` 并在登录页以提示条告知（不再静默报告"已退出"）。同时给凭据写盘加了**代次守卫**（`CredentialStore::generation` + `save_if`）：注销之后才到达的登录/续期写盘一律作废，不会再把 `auth.bin` 写回来。启动时若发现"主密钥已没了、密文还在"，只会报 `KeyringMissing` 并要求重新登录——不会再出现"注销后下次启动自动登录成上一个人"。

### 安全

- **依赖安全升级**：`vite` 6.4.4 → 6.4.5（npm advisory，开发依赖）、`tokio` 1.54.0 → 1.54.1（Cargo advisory）。两者都是 Dependabot 标记的 medium 级告警，升级后对应告警已关闭；无 API 与行为变化，`pnpm verify`、`cargo test` 与 `pnpm tauri build` 均通过。

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

[0.2.1]: https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.2.1
[0.2.0]: https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.2.0
[0.1.1]: https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.1.1
[0.1.0]: https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.1.0
