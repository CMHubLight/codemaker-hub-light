# 本地数据模型

> 这篇讲本机 SQLite 的位置与连接参数、8 张表的用途与写入时机、4 个迁移版本与 5 个 SQL 文件的对应关系、索引服务的查询，以及保留期清理；读者是想直接查库或改存储层的人。

## 数据库文件与连接

| 项 | 值 |
|---|---|
| 数据库文件 | `<数据目录>/codemaker-hub-light.db`（`db::DB_FILE_NAME`） |
| 数据目录 | `~/.codemaker-hub-light`（Windows 为 `%USERPROFILE%\.codemaker-hub-light`） |
| 日志模式 | WAL（每个连接建立时执行 `PRAGMA journal_mode = WAL`） |
| 其他 PRAGMA | `foreign_keys = ON`、`synchronous = NORMAL`、`busy_timeout = 5000` |
| 连接池 | r2d2 + r2d2_sqlite：`max_size = 8`、`min_idle = 1`、`connection_timeout = 5s` |

PRAGMA 不在迁移脚本里设置：迁移在事务中执行，而 `PRAGMA journal_mode = WAL` 不允许在事务内切换。统一放在 `db::pool::CONNECTION_PRAGMAS`，由每条新连接初始化时执行。

`Db::conn()` 从池里取连接本身可能阻塞（最长 5s），因此所有 async 命令与后台循环都必须经 `db::blocking::run` 把“取连接 + 查询”整体丢到阻塞线程池；直接在 `async fn` 里调用会占住 Tokio worker，让 IPC 与代理请求一起排队。

启动顺序是：打开数据库 → `db::run_migrations` → 注册 `AppState`。打开或迁移失败会弹窗并写 `logs/startup-error.log`，应用以错误退出。

内存库（单元测试）用 `file:memdb-<uuid>?mode=memory&cache=shared` 命名共享缓存，池上限 4；普通 `memory()` 会让池里每条连接各开一个空库，出现“第二条连接看不到表”。

## 表清单

```text name=db-tables
schema_migrations
auth_profiles
proxy_sessions
proxy_session_requests
proxy_request_costs
usage_daily
quota_snapshots
model_prices
```

## 逐表说明

| 表 | 用途 | 写入时机 |
|---|---|---|
| `schema_migrations` | 迁移版本记录 | 每次成功应用一个迁移版本时，与 DDL 同一事务写入 |
| `auth_profiles` | 非敏感账号档案（不含 token） | 登录成功、`/validate` 成功、退出登录 |
| `proxy_sessions` | 按会话 + 协议聚合的累计量 | 每个代理请求结束时 upsert |
| `proxy_session_requests` | 逐请求明细 | 每个代理请求结束时插入 |
| `proxy_request_costs` | 逐请求费用 | 与请求行同一事务插入 |
| `usage_daily` | 按日/协议/模型/客户端四维聚合 | 每个代理请求结束时 upsert |
| `quota_snapshots` | 额度快照（追加式） | 额度拉取成功并落库后追加（当前只写 `status = 'ok'`） |
| `model_prices` | 模型价格（元/百万 token） | 当前代码没有写入方（预留） |

### `schema_migrations`

`version INTEGER PRIMARY KEY`、`name TEXT`、`applied_at TEXT`。每个迁移版本一行；`repo::schema_version` 取 `MAX(version)`，空库返回 0，并作为 `app_info.schema_version` 暴露给前端。

### `auth_profiles`

只存非敏感身份信息：`user`（登录用户名）、`display_name`、`dept`、`server_url`、`auth_user`、`is_active`、`issued_at` / `expires_at` / `validated_at`（Unix 秒）、`last_error`、`models_json`（`/validate` 返回的模型目录缓存）、`created_at` / `updated_at`（RFC3339）。

- 写入：`repo::upsert_profile` 以 `(user, server_url)` 为冲突键 upsert，登录成功与 `validate` 成功都会调用（`is_active = 1`，`models_json` 刷新）；
- 读取：启动加载凭据后按 `active_profile` 补全 `display_name` / `dept` / `server_url` / 模型缓存；
- 退出登录：`repo::deactivate_profiles` 把 `is_active` 置 0，保留历史行，不删除。

### `proxy_sessions`

按 `external_session_id` + `api_format` 聚合。`external_session_id` 来自客户端会话头，缺省时由转发层 `proxy::forward::external_session_id` 生成 `anon:{client}:{date}`。累计字段包括 `request_count`、`error_count`、四类 token、`accumulated_cost_yuan`，以及 `first_seen_at` / `last_active_at`。

写入用单条 `INSERT ... ON CONFLICT(external_session_id, api_format) WHERE external_session_id IS NOT NULL DO UPDATE ... RETURNING id`：唯一索引是部分索引，冲突目标必须带同样谓词。用单条语句而不是“先查再插”，是为了两个请求同时首次进入时不撞唯一索引导致整个事务回滚。

### `proxy_session_requests`

一次请求一行。关键字段：`session_id`、`client_request_id`、`method`、`request_path`、`api_format`、`upstream_url`、`model`、`status_code`、`is_sse`、四类 token、`request_bytes` / `response_bytes`、`duration_ms` / `first_byte_ms`、`error_kind`、`request_headers`（头白名单 JSON，绝不含凭据）、`request_body` / `response_body`、`body_truncated`、`started_at` / `ended_at`、`day_local`、`status_kind`。

- 正文默认 `NULL`；开启 `record_body` 后按 64 KiB 单侧截断写入，`body_truncated` 标记是否截断；
- `day_local` 由 Rust 按机器本地时区从 `started_at` 换算，`status_kind` 由 `usage::aggregate::status_kind` 判定，二者都只在写入侧计算一次，列表筛选与界面标签共用。

### `proxy_request_costs`

一次请求一行（0004 起有唯一索引 `ux_prc_request`，旧库里的重复行会在迁移时按 `MAX(id)` 去重）。字段：`request_id`、`session_id`、`api_format`、`model`、四类 token、`cost_yuan`、`cost_source`（`upstream` / `estimated` / `none`）、`currency`（默认 `CNY`）、`total_credit`、`trace_id`、`detail_json`、`created_at`。`request_id` 外键在请求行删除时 `ON DELETE CASCADE`。

当前只有 `upstream` 与 `none` 两种来源；`estimated`（按 `model_prices` 估算）尚未接线。

### `usage_daily`

按 `(day, api_format, model, client)` 唯一聚合，字段为请求数、错误数、四类 token、`cost_yuan`、`total_duration_ms`、`updated_at`。每个代理请求结束时与请求行同一事务 upsert，累加各列。它是仪表盘“今日/本月”、日趋势、模型分布与 CSV 导出的数据源；`day` 与请求行的 `day_local` 同源，保证“今日 KPI”与“今天列表”口径一致。

### `quota_snapshots`

追加式快照：`profile_id`、`quota_type`（当前只用 `global`）、`status`（`ok` / `forbidden` / `not_found` / `error` / `unavailable`）、`http_status`、额度金额字段、`reset_day` / `period_start` / `reset_at` / `timezone` / `is_active`、`raw_json`（上游原始额度 JSON）、`fetched_at`。

读取只取 `status = 'ok'` 的最新一条（`fetched_at DESC, id DESC`）。当前写入方只在拉取成功后插入，`status` 固定为 `'ok'`、`http_status` 固定为 200；拉取失败只记日志、不写快照，所以上次成功值天然不会被覆盖。表结构保留了 `forbidden` / `not_found` / `error` / `unavailable` 等状态值，维护任务会清理超过 7 天的非 `ok` 行。`QuotaInfo` 里的云端月度分布（`app_tokens` / `model_tokens` 等）是从 `raw_json` 的 `current_month` 解析出来的。

### `model_prices`

模型单价表（元/百万 token，四类 token 分别计价 + `currency`、`source`、`effective_from`、`updated_at`），唯一键 `model`。迁移创建了表，但当前代码只有 `repo::list_model_prices`（标注 P3）没有写入方，费用估算路径也未启用，实际是空表。

## 迁移机制

版本号定义在 `src-tauri/src/db/mod.rs` 的 `MIGRATIONS` 常量里：每个 `Migration { version, name, steps }`，每个 `Step { sql, probe }`，SQL 用 `include_str!` 内嵌 `migrations/*.sql`。

`run_migrations` 的流程：

1. `target = latest_version()`（列表最后一个版本号）；
2. 快路径：`SELECT MAX(version)` 已 `>= target` 直接返回，绝大多数启动只花一次查询；
3. 慢路径逐版本：版本行已存在则跳过；否则开一个事务，逐步执行 `steps`；
4. 每个 step 先跑 `probe`：命中说明结构已在，跳过该语句；探测 SQL 自身报错按“未生效”处理，交给正式 DDL 报真实错误；
5. 执行完把 `(version, name, applied_at)` 用 `INSERT OR REPLACE` 写进 `schema_migrations`，提交事务。

每个版本一个事务，所以当前代码里 DDL 与版本行同时提交。仍然需要逐语句探测 + 可重试，是因为旧版本构建可能留下“结构已生效、版本行没写”的库（例如旧实现不是每版本一个事务），而迁移失败会让应用**每次启动都失败**：0003 曾经有两条 `ALTER TABLE` 却只探一条，如果在“`day_local` 已提交、`status_kind` 未提交”的窗口崩溃，重跑会撞 `duplicate column`。现在每个 step 各自探测：已存在的跳过，缺的补上。幂等语句（`CREATE ... IF NOT EXISTS`、带 `WHERE ... IS NULL` 的回填、去重 `DELETE`）不需要 `probe`。

### 4 个版本与 5 个 SQL 文件

| 版本 | name | SQL 文件 | 步骤数 | 探测 |
|---|---|---|---|---|
| 1 | `0001_init` | `0001_init.sql` | 1 | 无（全部 `IF NOT EXISTS`，天然幂等） |
| 2 | `0002_auth_models` | `0002_auth_models.sql` | 1 | `auth_profiles.models_json` 列是否存在 |
| 3 | `0003_request_day_status` | `0003_request_day_status.sql` + `0003_request_status_kind.sql` | 2 | 分别探测 `day_local`、`status_kind` 列 |
| 4 | `0004_usage_perf` | `0004_usage_perf.sql` | 1 | 最后一条语句建出的 `ux_prc_request` 索引 |

版本 3 拆成两个可独立探测的步骤，就是上节说的“崩溃窗口”修复：任一列缺失只补那一列，两列都在就只补版本行。

版本 4 是一个可整批重跑的批次：DDL 全部 `IF NOT EXISTS` / `IF EXISTS`，去重 `DELETE` 在无重复行时影响 0 行，因此用“最后建成的对象”作探测，命中即视为整批完成。

相关测试覆盖了这些路径：`migrations_create_all_tables`、`migrations_are_idempotent`、`migrations_retry_after_partial_0002`、`migrations_reapply_when_column_missing`、`migrations_retry_after_partial_0003`、`migration_0003_partial_retry_heals_day_local_only`、`migration_0004_partial_retry_heals`、`migration_0003_backfills_legacy_rows`。

## 索引清单

| 索引 | 版本 | 服务的查询 / 约束 |
|---|---|---|
| `ux_auth_profiles_user_server` | 0001 | `upsert_profile` 的冲突目标：同一 `(user, server_url)` 只有一行 |
| `ix_auth_profiles_active` | 0001 | `active_profile`：`is_active = 1` 按 `updated_at DESC` 取一条 |
| `ux_proxy_sessions_external` | 0001 | recorder 会话 upsert 的冲突目标（部分索引，仅非空会话） |
| `ix_proxy_sessions_last_active` | 0001 | 维护任务按 `last_active_at < cutoff` 清理会话 |
| `ix_psr_started_at` | 0001 | 单列开始时间倒序检索；当前列表查询已改用 0004 的 keyset 索引，此索引保留 |
| `ix_psr_session` | 0001 | 会话 → 请求列表；当前代码没有按 `session_id` 的查询使用它 |
| `ix_prc_request` | 0001 | 按 `request_id` 查费用；0004 另加唯一索引后功能重叠但保留 |
| `ix_prc_created_at` | 0001 | 费用按时间倒序（预留；当前详情按 `request_id` 定位） |
| `ix_prc_model` | 0001 | 按模型汇总费用（预留） |
| `ux_usage_daily_dim` | 0001 | `usage_daily` upsert 冲突目标：四维唯一 |
| `ix_usage_daily_day` | 0001 | 日区间查询；0004 的覆盖索引以 `day` 打头，功能重叠但保留 |
| `ix_quota_snapshots_latest` | 0001 | 设计给按 `profile_id` 查最新快照；真实查询不带 `profile_id`，0004 补了部分索引 |
| `ux_model_prices_model` | 0001 | 模型价格按 `model` 唯一 |
| `ix_quota_snapshots_ok_latest` | 0004 | `latest_quota_snapshot`：`quota_type` + `status='ok'` 按 `fetched_at, id` 倒序 `LIMIT 1`（部分索引） |
| `ix_quota_snapshots_type_id` | 0004 | 维护按 `quota_type` 保留最近 N 条时定位 `id` 边界 |
| `ix_psr_day_started_id` | 0004 | 请求列表 keyset：`ORDER BY day_local DESC, started_at DESC, id DESC` 的区间反向扫描 |
| `ix_psr_status_day_started` | 0004 | 状态筛选：等值列 `status_kind` 打头，后接日区间与排序键 |
| `ix_psr_format_day_started` | 0004 | 协议筛选：等值列 `api_format` 打头 |
| `ix_usage_daily_day_cover` | 0004 | 日区间 `SUM` 的覆盖索引（index-only scan） |
| `ux_prc_request` | 0004 | 一请求一行费用，保证详情 `LEFT JOIN` 不重复 |

被删除或替换的索引：`ix_psr_status`（0003 第 2 步删除，筛选改用 `status_kind`）、`ix_psr_day_local` 与 `ix_psr_status_kind`（0004 删除，已被 keyset/复合索引前缀覆盖）。迁移失败只影响启动，索引的收益由 `EXPLAIN QUERY PLAN` 单测守住（列表、游标、状态筛选、计数、额度最新快照都断言命中索引且无全表扫描）。

## 维护任务

清理由 `db/maintenance.rs` 提供，`bootstrap::spawn_maintenance` 负责调度：启动后立即跑一次，之后每 24h 一次（`MissedTickBehavior::Delay`，休眠恢复后最多补一次）。每轮从设置快照读取 `retention_days`，所以改完设置下一轮即生效，无需重启。

边界日计算：`cutoff = 本地今天 - (retention_days - 1)`，格式 `YYYY-MM-DD`；`< cutoff` 的数据被删除，等于边界日的保留。

| 目标表 | 删除条件 | 批次 | 说明 |
|---|---|---|---|
| `proxy_session_requests` | `day_local IS NOT NULL AND day_local < cutoff` | 5000 行/批 | 费用行靠 `ON DELETE CASCADE` 一起删；`day_local IS NULL` 的异常行一律保留 |
| `proxy_sessions` | `last_active_at < cutoff` | 5000 行/批 | 请求行先删，剩下的是保留期内还有活动的会话 |
| `usage_daily` | `day < cutoff` | 5000 行/批 | 与请求表同一保留窗口 |
| `quota_snapshots` | 先删 `status <> 'ok'` 且 `fetched_at` 早于 7 天前；再按 `quota_type` 只保留最近 200 条 | 单条语句 | 成功快照只增不删，用条数上限兜底 |

分批删除是为了不长时间持写锁把代理落库饿死；批间自动提交。任何一张表有删除时最后执行一次 `ANALYZE` 刷新统计，无删除则跳过。结果写进 `MaintenanceReport`（`cutoff_day`、`retention_days`、各表删除行数）并打 info 日志“保留期清理完成”。重复执行必须零变更，测试 `retention_prunes_old_rows_and_keeps_boundary` 覆盖了边界日、NULL 行与幂等性。

不参与清理的数据：`auth_profiles`（退出登录只置 `is_active = 0`，保留历史）、`model_prices`、`schema_migrations`，以及 `exports/` 下已导出的 CSV。

## 不变量：任何 token / auth_key 都不入库

这条约束贯穿整库：

- 8 张表里没有 `token`、`auth_key`、`access_token` 之类的列；凭据只存在 `auth.bin`（加密文件）与系统凭据库中，二者都在 SQLite 之外；
- `proxy_session_requests.request_headers` 只写头白名单 JSON，绝不包含 `authorization` / `x-api-key` 等凭据头；
- recorder 只接收 token 计数与费用，不接收任何凭据；日志与事件同样不携带凭据；
- `auth_profiles` 的 `models_json` 是模型目录缓存，不含令牌。

`migrations/0001_init.sql` 顶部的安全约定与 `src/lib/types.ts` 的注释都显式写了这条规则，改表结构时必须保持。

## 源码索引

| 路径 | 用到的内容 |
|---|---|
| `src-tauri/src/db/mod.rs` | `MIGRATIONS`、`Step`/`probe`、`run_migrations`、`schema_version` |
| `src-tauri/src/db/pool.rs` | 数据库文件名、`CONNECTION_PRAGMAS`、池参数、内存库 |
| `src-tauri/src/db/blocking.rs` | 异步 → 阻塞边界与 5s 取连接语义 |
| `src-tauri/src/db/repo.rs` | profile / 额度快照 / 用量 / 请求明细的读写与查询计划单测 |
| `src-tauri/src/db/rows.rs` | 行结构与按列名解码 |
| `src-tauri/src/db/maintenance.rs` | 保留期裁剪、批次、`ANALYZE`、额度快照剪枝 |
| `src-tauri/migrations/0001_init.sql` | 8 张表与初始索引、安全约定 |
| `src-tauri/migrations/0002_auth_models.sql` | `models_json` 列 |
| `src-tauri/migrations/0003_request_day_status.sql`、`0003_request_status_kind.sql` | 版本 3 的两个步骤与历史回填 |
| `src-tauri/migrations/0004_usage_perf.sql` | keyset/复合/覆盖索引、去重与唯一索引 |
| `src-tauri/src/proxy/recorder.rs` | 会话/请求/费用/日聚合的单事务写入 |
| `src-tauri/src/usage/aggregate.rs` | `status_kind` 唯一定义、`RecentRequest` 映射 |
| `src-tauri/src/usage/queries.rs` | 汇总/趋势/分页/导出的查询与事务 |
| `src-tauri/src/auth/service/persist.rs` | profile upsert 与额度快照写入时机 |
| `src-tauri/src/auth/service/logout.rs` | 登出时 `deactivate_profiles` |
| `src-tauri/src/bootstrap.rs` | 维护任务调度周期 |
| `src-tauri/src/state.rs` | `db_path` / `exports_dir` 路径拼接 |
