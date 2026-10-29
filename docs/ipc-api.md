# IPC 命令与事件

> 这篇讲前端能调用的 19 个 Tauri 命令与 6 个后端事件：参数、返回类型、超时语义、典型错误、触发时机，以及浏览器预览时的 mock 行为；读者是改前端、写自动化或排查界面取数问题的人。

## 约定

### 参数命名

- **命令顶层参数名一律 camelCase**：Rust 的 `from_day` / `to_day` 在前端写成 `fromDay` / `toDay`（Tauri 默认把参数名转成 lowerCamelCase）。写成 `snake_case` 不会报错，但 `Option<T>` 参数会静默收到 `None`，表现为“筛选条件没生效”。
- **嵌套 payload 保持 snake_case**：`filter`（`RequestFilter`）与 `settings`（`SettingsPatch`）走 serde 反序列化，字段名与 Rust 结构一致。
- 命令返回结构与 `src/lib/types.ts` 对齐，全部 `snake_case`。

### 超时语义

统一入口是 `src/lib/ipc.ts` 的 `invokeCmd`：Tauri 桌面端走真实 `invoke` 并与超时竞速，浏览器预览走内存 mock。

| 命令 | 等待上限 |
|---|---|
| 默认（未单独列出） | 10s（`DEFAULT_COMMAND_TIMEOUT_MS`） |
| `auth_start_login`、`auth_submit_deep_link`、`auth_retry_renewal`、`quota_get`、`proxy_restart`、`usage_export_csv` | 30s |
| `app_open_logs` | 15s |
| `auth_poll_status` | 10s（显式配置，与默认一致） |

超时只让**界面止损**，不取消后端任务：Tauri 的 `invoke` 没有取消机制，写操作（导出 CSV、重启代理、保存设置）可能仍在后台跑完。超时抛 `CommandTimeoutError`，消息包含命令名与秒数，提示“若这是写操作，它可能仍在后台执行”。

### 错误分类

后端错误是普通字符串（`Result::Err(String)`），没有结构化错误码；前端 `describeError` 按文本保守分类为 `forbidden`、`not_found`、`unauthenticated`、`network`、`timeout`、`unknown`，用于选择不同提示文案，不改变重试策略。

## 命令清单

```text name=ipc-commands
app_info
app_open_logs
settings_get
settings_update
auth_status
auth_start_login
auth_poll_status
auth_logout
auth_refresh_validate
auth_retry_renewal
auth_submit_deep_link
quota_get
proxy_status
proxy_restart
usage_summary
cost_trend_get
usage_requests
usage_request_detail
usage_export_csv
```

命令在 `src-tauri/src/lib.rs` 的 `generate_handler!` 中注册。前端 `CommandName` 联合类型目前只收录 17 个：`auth_refresh_validate` 与 `usage_request_detail` 已注册但未进入该类型、界面也未调用（`auth_refresh_validate` 的注释写明“预留给设置页检查登录状态”）。

## 命令明细

### 应用与设置

| 命令 | 入参 | 返回 | 超时 | 典型错误 |
|---|---|---|---|---|
| `app_info` | 无 | `AppInfo` | 10s | `打开数据库失败：…`、`读取 schema 版本失败：…` |
| `app_open_logs` | 无 | `OpenLogsResult` | 15s | `创建日志目录失败：…`、`打开日志目录失败：…` |
| `settings_get` | 无 | `Settings` | 10s | 无（读内存态） |
| `settings_update` | `settings: SettingsPatch`（前端 `Partial<AppSettings>`） | `Settings` | 10s | `设置不合法：<原因>`、`保存设置失败：<IO/序列化原因>` |

- `app_open_logs` 会先建 `logs/`，再按平台调用 `explorer` / `open` / `xdg-open`。浏览器预览里返回 `opened: false`。
- `settings_update` 是局部补丁：未提交的字段保持原值；校验 → 原子落盘 → 替换内存 → 写回代理策略，任一步失败都不改变内存与运行期策略。字段与校验见设置项文档。

### 认证与额度

| 命令 | 入参 | 返回 | 超时 | 典型错误 |
|---|---|---|---|---|
| `auth_status` | 无 | `AuthStatus` | 10s | 一般无错误；懒加载凭据失败会体现在 `message` / `credentials_valid` |
| `auth_start_login` | 无 | `AuthLoginStart` | 30s | `网关地址无效：…`、`认证网络请求失败：…` |
| `auth_poll_status` | 无 | `AuthStatus` | 10s | `认证网络请求失败：…`、`登录会话已结束…` |
| `auth_logout` | 无 | `AuthStatus` | 10s | 无（本地清理） |
| `auth_refresh_validate` | 无 | `AuthStatus` | 10s | `尚未登录`、`凭据被服务端拒绝：…`、网络错误 |
| `auth_retry_renewal` | 无 | `AuthStatus` | 30s | `网关地址无效：…`；续期失败原因体现在 `message`/`offline_grace` |
| `auth_submit_deep_link` | `url: string` | `AuthStatus` | 30s | `回调链接无效：…`、未绑定当前登录会话的拒绝提示 |
| `quota_get` | `force?: boolean` | `QuotaInfo \| null` | 30s | `尚未登录`、`上游返回 HTTP 403/404…`、网络错误 |

- `auth_status` 首次调用会懒加载 `auth.bin`（失败不抛错，只写状态），有凭据时确保后台循环已启动。
- `auth_start_login` 让 Rust 侧生成待登录会话并用系统浏览器打开登录页；浏览器打开失败不算命令失败，用 `opened_browser: false` 让界面展示可复制的登录链接。返回里的 `poll_interval_secs = 2`、`timeout_secs = 300`。
- `auth_poll_status` 由登录页每 2s 调用；只有状态真的变化或额度更新时才广播事件，避免空转事件触发整页重取。连续失败 3 次才由界面判失败。
- `auth_submit_deep_link` 是深链 scheme 未注册/浏览器未自动拉起时的兜底：前端只提交用户粘贴的 `codemakerhub-light://auth/callback?...`，解析、校验、保存凭据全在 Rust 侧，token 不回传前端。
- `auth_retry_renewal` 是手动“重新检测/续期”：强制用 `auth_key` 续期一次（跳过后台 60s 检查节流），成功后补一次 `/validate` 刷新账号档案与模型目录；失败原因写入返回的 `AuthStatus.message`，认证类事件同时广播。
- `auth_logout` 停止后台循环、删除 `auth.bin` 与系统凭据库中的主密钥、清内存状态，并把数据库里的账号档案置为非激活。
- `quota_get` 默认只读本地最新成功快照（不打网关）；`force: true` 时请求上游刷新。没有任何成功快照时返回 `null`，不伪造 0。

### 代理

| 命令 | 入参 | 返回 | 超时 | 典型错误 |
|---|---|---|---|---|
| `proxy_status` | 无 | `ProxyStatus` | 10s | 无（未运行时用设置里的期望值填充展示字段） |
| `proxy_restart` | 无 | `ProxyStatus` | 30s | `重启代理失败：<绑定/回环校验原因>` |

`proxy_restart` 的流程是：停止旧任务（最多等 5s）→ 用当前设置重新绑定 → 回写 `proxy.port` → 广播 `proxy://status`。只允许回环绑定；端口被占用时按设置的端口逐个回退，最后交给系统分配。

### 用量

| 命令 | 入参 | 返回 | 超时 | 典型错误 |
|---|---|---|---|---|
| `usage_summary` | 无 | `UsageSummary` | 10s | `读取用量汇总失败：…` |
| `cost_trend_get` | `range: string`（`24h` / `30d`） | `CostTrend` | 10s | `不支持的费用曲线范围：…`、`读取本机费用趋势失败：…` |
| `usage_requests` | `filter?: RequestFilter` | `RequestPage` | 10s | `读取请求列表失败：…`（含 `未知的协议筛选值` / `未知的状态筛选值`） |
| `usage_request_detail` | `id: number` | `RequestDetail \| null` | 10s | `读取请求详情失败：…` |
| `usage_export_csv` | `fromDay?: string`、`toDay?: string` | `UsageExportResult` | 30s | 日期参数错误返回 `Err`；导出失败返回 `Ok(saved: false, message: …)` |

- `usage_summary` 在一个读事务里跑 5 个查询（今日、本月、近 30 天、本月模型分布、最近 10 条请求），保证页面内数字同快照。
- `cost_trend_get("24h")` 打上游小时费用接口，返回积分（`source = cloud`、`unit = points`）；`"30d"` 读本机 `usage_daily`，返回人民币（`source = local`、`unit = cny`），缺失日期补 0。
- `usage_requests` 默认 `limit = 50`（上限 200），`offset` 上限 100000；优先用 `after` 游标（keyset），`include_total = false` 时跳过 `COUNT` 并让 `total = -1`。排序键是 `(day_local, started_at, id)`，游标原样传回即可翻页。
- `usage_request_detail` 返回正文、费用明细等；正文默认未记录时为 `null`。该命令目前界面未使用。
- `usage_export_csv` 默认导出最近 30 天（含今天），只按日期区间过滤、不套用列表里的协议/状态筛选；写到 `exports/usage_<from>_<to>.csv`，带 UTF-8 BOM，单次上限 20 万行，超出时 `truncated = true`。

### 主要返回类型

| 类型 | 关键字段 |
|---|---|
| `AppInfo` | `name`、`version`、`tauri_version`、`platform`、`arch`、`identifier`、`data_dir`、`log_path`、`logs_dir`、`db_path`、`proxy_port_file`、`gateway_url`、`proxy_port`、`schema_version` |
| `Settings`（前端 `AppSettings`） | `gateway_url`、`proxy_bind`、`proxy_port`、`record_body`、`retention_days`、`credit_to_yuan` |
| `AuthStatus` | `state`（`unauthenticated`/`pending`/`authenticated`/`reauth_required`/`error`）、`user`、`display_name`、`expires_at`、`offline_grace`、`message`，以及诊断字段 `dept`、`server_url`、`issued_at`、`credentials_valid`、`pending_poll_uuid`、`pending_login_started_at`、`deep_link_received`、`last_validated_at` |
| `AuthLoginStart`（前端 `LoginSession`） | `login_url`、`poll_uuid`、`redirect_uri`、`poll_interval_secs`、`timeout_secs`、`opened_browser`、`hint` |
| `QuotaInfo` | 额度金额与百分比、`reset_day`、`reset_at`、`period_start`、`is_active`、`usage_cost`、`fetched_at`、`status`，以及云端月度分布 `total_tokens`、`app_tokens`、`app_costs`、`model_tokens`、`model_costs` |
| `ProxyStatus` | `running`、`host`、`port`、`base_url`、`gateway_url`、`last_error`、`started_at`、`uptime_secs`、`request_count`、`error_count`、`port_file`、`health_url` |
| `UsageSummary` | `today` / `month`（`UsageTotals`）、`daily`（近 30 天）、`models`、`recent`（最近 10 条）、`generated_at` |
| `UsageTotals` | `requests`、四类 token、`total_tokens`、`cost_yuan`、`cache_hit_rate`、`avg_latency_ms`、`error_rate`（无数据为 `null`） |
| `RecentRequest` | `id`、`created_at`、`model`、`protocol`、`client`、`status`、`latency_ms`、`first_byte_ms`（首个上游响应字节，仅流式请求有值）、`input_tokens`、`output_tokens`、`cost_yuan` |
| `RequestPage` | `total`（`-1` 表示未统计）、`items`、`next_cursor`、`prev_cursor` |
| `UsageExportResult` | `saved`、`path`、`message`、`rows`、`bytes`、`truncated` |
| `OpenLogsResult`（前端 `AppLogResult`） | `opened`、`message`、`path` |

## 事件清单

```text name=ipc-events
auth://state
auth://callback
quota://updated
proxy://status
proxy://error
app://second-instance
```

## 事件明细

| 事件 | payload | 触发时机（Rust 发射点） |
|---|---|---|
| `auth://state` | `AuthStatus` | `auth_poll_status` 检测到状态变化或额度更新；`auth_start_login` 发出 pending；`auth_submit_deep_link` / `auth_refresh_validate` / `auth_retry_renewal` / `auth_logout` 完成后；深链处理成功或被拒绝；后台 validate/续期/额度循环；启动期凭据加载失败。前端直接用它覆盖登录态，不触发数据重取 |
| `auth://callback` | `AuthCallbackEvent { poll_uuid: string \| null, has_credentials: boolean }` | 深链解析成功后立即发（无论是否带凭据）；事件只含非敏感字段，绝不含 token。登录页用它立刻补一次轮询，应用外壳用它重读登录态 |
| `quota://updated` | `QuotaInfo` | `quota_get` 带 `force: true` 且拿到快照；登录/深链完成时额度确实更新；后台额度循环发现新快照。前端用它刷新额度卡与 24h 云端曲线 |
| `proxy://status` | `ProxyStatus` | 启动后台代理成功；`proxy_restart` 成功。前端刷新设置页代理状态与用量数据 |
| `proxy://error` | `ProxyErrorEvent { message: string }` | 启动后台代理失败时发一次（应用继续运行，可调 `proxy_restart` 重试） |
| `app://second-instance` | `{}`（`Record<string, never>`） | 单实例插件检测到第二次启动：后端聚焦主窗口并转发深链参数，同时发空对象事件；前端重读登录态并刷新全部数据 |

事件订阅统一走 `useTauriEvent`：浏览器预览（无 Tauri）下自动跳过；订阅失败（权限或后端未就绪）静默降级，命令拉取仍然有效。

## 页面 / 功能 → 命令与事件

| 页面 / 功能 | 用到的命令 | 订阅的事件 |
|---|---|---|
| 应用外壳（启动、登录态、退出登录、失败屏） | `auth_status`、`auth_logout`、`app_open_logs` | 全部 6 个（合并刷新后再分发） |
| 登录页 | `auth_start_login`、`auth_poll_status`、`auth_submit_deep_link` | `auth://state`、`auth://callback` |
| 概览 | `quota_get`（普通读与手动 `force` 刷新）、`usage_summary`、`cost_trend_get` | `quota://updated` 驱动额度与 24h 曲线；其余事件经合并刷新 |
| 请求 | `usage_requests`、`usage_export_csv` | 无直接订阅，由外壳的合并刷新触发表格回第一页 |
| 设置 | `settings_get`、`settings_update`、`proxy_status`、`proxy_restart`、`app_info`、`app_open_logs`、`auth_retry_renewal`（账户面板手动续期） | 无直接订阅，代理状态与应用信息按外壳 revision 刷新 |
| 未接入界面 | `auth_refresh_validate`、`usage_request_detail` | — |

## 浏览器预览（无 Tauri）时的 mock

`isTauri()` 用 `window.__TAURI_INTERNALS__ !== undefined` 判断运行环境。`pnpm dev` 的普通浏览器里没有该对象，`invokeCmd` 直接走 `mockInvoke`，不经过 `withTimeout`（mock 自己延迟 120ms），因此 `pushState`、刷新页面即可重置全部状态。

mock 的关键行为：

- **地址一律是占位值**：`gateway_url = https://api.example.test`（设置、代理状态、应用信息），代理 `base_url = http://127.0.0.1:15721`、端口 `15721`，不访问任何真实上游；
- 登录：`auth_start_login` 返回 `login_url: ''`，登录页据此提示“需在桌面端登录”；认证类命令统一返回 `unauthenticated` 状态的 mock 快照（含 `mock-poll-uuid`）；
- 额度：`quota_get` 返回 `null`，页面按“暂无额度数据”处理；
- 用量：`usage_summary` 返回全 0 且比率为 `null`；`cost_trend_get` 回显传入的 `range`，`24h` 标为 `cloud/points`、`30d` 标为 `local/cny`，`points` 为空；`usage_requests` 返回 `total: 0`；
- 写操作：`settings_update` 直接把补丁合并进内存（不做后端校验），`proxy_restart` 把 mock 状态改成运行中并重置 `started_at` / `uptime_secs`，`usage_export_csv` 返回 `saved: false`（“浏览器预览模式没有本地数据库，导出不可用”），`app_open_logs` 返回 `opened: false`；
- 事件：`useTauriEvent` 在 `isTauri()` 为假时直接 return，不发起 `listen`。

## 源码索引

| 路径 | 用到的内容 |
|---|---|
| `src-tauri/src/lib.rs` | `generate_handler!` 的 19 个命令、单实例事件发射 |
| `src-tauri/src/commands/app.rs` | `app_info` / `app_open_logs` 的返回结构与错误 |
| `src-tauri/src/commands/settings.rs` | `settings_get` / `settings_update` |
| `src-tauri/src/commands/auth.rs` | 认证命令、`auth://state` 与 `quota://updated` 发射点 |
| `src-tauri/src/commands/proxy.rs` | `proxy_status` / `proxy_restart`、`proxy://status` |
| `src-tauri/src/commands/usage.rs` | 用量命令的参数校验、导出区间与返回 |
| `src-tauri/src/bootstrap.rs` | `proxy://status` / `proxy://error`、启动期 `auth://state` |
| `src-tauri/src/deep_link.rs` | `auth://callback` 发射与 `auth://state` 补发 |
| `src-tauri/src/auth/service/loops.rs` | 后台循环发射 `auth://state` / `quota://updated` |
| `src-tauri/src/auth/mod.rs` | 轮询间隔/超时常量、认证错误文案 |
| `src-tauri/src/proxy/server.rs` | `ProxyStatus` 字段 |
| `src-tauri/src/usage/mod.rs` | `UsageSummary` / `RequestPage` / `QuotaInfo` 等返回结构 |
| `src/lib/ipc.ts` | `invokeCmd`、超时表、错误分类、mock 数据与地址 |
| `src/lib/events.ts` | `AppEventName` 与订阅降级 |
| `src/lib/types.ts` | 全部 IPC 契约类型 |
| `src/lib/async.ts` | `useCommand` / `useAsyncResource` 的调用与取消语义 |
| `src/App.tsx`、`src/pages/Login.tsx`、`src/pages/dashboard/index.tsx`、`src/pages/Requests.tsx`、`src/pages/settings/index.tsx` | 页面实际调用关系 |
