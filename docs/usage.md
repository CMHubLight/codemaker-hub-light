# 用量统计

> 这篇讲代理把请求写进本地库的时机与字段、token 四元组的归一规则、费用从哪里来怎么换算，以及账号级与本机级两种口径的区别、本地日、聚合保留、分页与 CSV 导出规则；给要读用量数字或改统计代码的维护者看。

## 1. 请求是怎么被记录的

一次代理请求从进入 `forward_handler` 起就有一份 `RequestContext`（请求 id、方法、客户端路径、协议、上游 URL、模型、客户端、会话标识、是否 SSE、请求体大小、开始时间）。记录发生在响应中继结束之后，同一个生产者任务里收尾：

| 时机 | 触发位置 | 结果 |
| --- | --- | --- |
| 正常结束（含 SSE 流结束、客户端断开、翻译收尾） | `spawn_producer` 末尾调用 `record_completion` | 写一条完整记录 |
| 早期失败（401 无凭据、413 超限、400 翻译失败、504 首字节超时） | `record_early_failure` | 写一条「无 usage、无费用」的失败记录 |
| 上游连接失败 / 首字节超时 / 上游 3xx | `forward_handler` 内直接调用 | 同上，状态码按实际错误给 |

落库在 `db::blocking::run` 里跑，整个事务完成才返回；写失败只记一条 `warn`，不影响已经发给客户端的响应。正文默认不落库，开启后按 64 KB（`DEFAULT_MAX_BODY_BYTES`）截断并置 `body_truncated`。

一次事务写四张表：

| 表 | 粒度 | 关键列 |
| --- | --- | --- |
| `proxy_sessions` | 会话 × 协议 | `external_session_id`、`request_count`、`error_count`、四个 token 累加列、`accumulated_cost_yuan`、`first_seen_at`、`last_active_at` |
| `proxy_session_requests` | 一条请求一行 | `session_id`、`request_path`、`api_format`、`upstream_url`、`model`、`status_code`、`is_sse`、四个 token、`request_bytes`、`response_bytes`、`duration_ms`、`first_byte_ms`、`error_kind`、`day_local`、`status_kind`、可选正文 |
| `proxy_request_costs` | 一条请求一行 | `cost_yuan`、`cost_source`、`currency`、`total_credit`、`trace_id`、`detail_json` |
| `usage_daily` | 日 × 协议 × 模型 × 客户端 | `request_count`、`error_count`、四个 token、`cost_yuan`、`total_duration_ms` |

会话标识优先取客户端会话头，没有则用 `anon:{client}:{本地日期}`；客户端类型由 User-Agent 识别（claude / codex / opencode / kimi，其余 `unknown`）。`proxy_sessions` 与 `usage_daily` 都用单条 UPSERT 累加，避免并发首请求撞唯一索引导致整条事务回滚。

## 2. token 四元组归一

`TokenUsage` 只有四个桶，`total()` 是四者相加：`input_tokens`（**未命中缓存**的输入）、`output_tokens`、`cache_read_tokens`、`cache_write_tokens`。各协议的上游字段名不同，归一规则如下：

| 协议 | usage 位置 | 输入 | 输出 | 缓存读 / 写 |
| --- | --- | --- | --- | --- |
| Anthropic Messages | `message.usage` 或顶层 `usage` | `input_tokens` 原值 | `output_tokens` | `cache_read_input_tokens` / `cache_creation_input_tokens` |
| OpenAI Chat | 顶层 `usage` | `prompt_tokens - cache_read`（下限 0） | `completion_tokens` | `prompt_tokens_details.cached_tokens`（缺省用 `prompt_cache_hit_tokens`）/ `prompt_tokens_details.cache_write_tokens`（缺省 0） |
| OpenAI Responses | `response.usage` 或顶层 `usage` | `input_tokens - cache_read`（下限 0） | `output_tokens` | `input_tokens_details.cached_tokens` / `input_tokens_details.cache_write_tokens` |

OpenAI 系的 `prompt_tokens` / `input_tokens` 本身包含缓存命中部分，所以归一后必须减掉，否则 `input + cache_read` 会重复计数。

数字解析一律宽松：`as_i64` → `as_u64` → `as_f64` → 字符串 `parse`，失败记 0。SSE 逐事件解析后用 `merge` 合并，同一字段取**最大值**：Anthropic 的 `message_start` 只有输入，最终输出在 `message_delta`，取最大值正好得到最终计数。

请求状态由 `usage::aggregate::status_kind` 唯一定义，落库列、界面标签、筛选条件共用它：

| `error_kind` | `status_code` | `status_kind` |
| --- | --- | --- |
| 含 `disconnect` / `cancel` / `abort` | 任意 | `cancelled` |
| 其它非空值 | 任意 | `error` |
| 空 | 200–399 | `success` |
| 空 | 其它非空值 | `error` |
| 空 | NULL | `unknown` |

代理实际写入的 `error_kind` 只有七种：`timeout`、`upstream_error`、`upstream_redirect`、`client_aborted`、`translation_error`、`unauthorized`、`request_body`。

## 3. 费用来源与换算

费用只看上游响应里的 `aigw.cost.total_credit`，也接受被包在 `usage` 下的 `/usage/aigw/cost/total_credit`；数字与数字字符串都接受（上游可能返回字符串）。

```text
cost_yuan = round(total_credit × credit_to_yuan, 8)
```

- `credit_to_yuan` 是设置键，默认 `0.01`（即 1 积分 = 0.01 元，等价于 `total_credit / 100`），保存后下一次请求立即生效，不需要重启代理。
- 换算结果保留 8 位小数（`(value * 1e8).round() / 1e8`），非有限数一律归 0。
- 上游没给 `total_credit` 时：`total_credit = null`、`cost_yuan = 0.0`、`cost_source = none`，即**记 0 而不是留空**，界面据此显示 ¥0.0000。
- `trace_id` 取 `aigw.trace_id`（同样接受 `usage` 包裹），只做展示与排障。
- `currency` 固定写 `CNY`。

| `cost_source` | 何时产生 | 当前是否存在 |
| --- | --- | --- |
| `upstream` | 解析到 `aigw.cost.total_credit` | 是 |
| `estimated` | 预留：按 `model_prices` 估算 | 否，解析路径不会产出 |
| `none` | 上游没给费用（含所有早期失败记录） | 是 |

## 4. 两种口径：账号级与本机级

概览页把两套完全不同的数据摆在一起，必须分开读。

| 维度 | 账号级（云端） | 本机级（本地库） |
| --- | --- | --- |
| 数据来源 | `GET /api/v1/user_quota/usage?quota_type=global`、`GET /vega/user_hourly_cost` | `usage_daily`、`proxy_session_requests` |
| 覆盖范围 | 该账号在上游网关的全部用量（含不经本机代理的客户端） | 只覆盖经本机代理转发的请求 |
| 时间窗 | 月度额度由服务端 `period_start` / `reset_day` 决定；小时费用是最近 24 小时快照 | 今天 / 本地月 1 号起 / 最近 30 天，全部按**本地日** |
| 单位 | 积分（额度、已用、剩余、本月消耗、模型分布、24h 曲线） | 元（今日费用、30 天曲线、请求明细、CSV） |
| 存储 | `quota_snapshots` 只存快照（含 `raw_json`），不参与本地聚合 | `usage_daily` 是唯一聚合源 |
| 能否相加 | 不能 | 不能 |

三个不能互相验证的原因：

1. 覆盖面不同：本机级只统计经过代理的流量，账号级包含该账号在其它客户端、其它机器上的消耗，本机级通常小于账号级。
2. 月界不同：账号级的「本月」由上游的 `period_start` / `reset_day` 决定，本机级的「本月」固定为本地时区 1 号 00:00 起。
3. 单位与口径不同：账号级是上游积分，本机级是「上游积分 × 用户设置的换算系数」得到的人民币；系数被改过之后，历史行不会重算。

因此界面上「今日费用（¥，本机）」与「本月消耗（积分，云端）」并列展示纯属排版，不能拿来互相校对，也不能相减求差。

## 5. 本地日与历史回填

「本地日」全应用只有一个定义：`usage::local_day` = UTC 时刻转本地时区后取 `%Y-%m-%d`。

| 位置 | 用法 |
| --- | --- |
| `proxy_session_requests.day_local` | 落库时由请求**开始时间**换算（不是结束时间，因为列表按开始时间排序） |
| `usage_daily.day` | 与同一请求的 `day_local` 同源，保证「今日 KPI」和「今天列表」口径一致 |
| 请求列表筛选 | 按 `day_local` 区间比较，不按 `started_at` 前缀 |
| keyset 游标 | 直接带回库里存的 `day_local`，不现场重算 |

历史数据回填（迁移 `0003_request_day_status.sql`）：新增 `day_local` 后对历史行执行

```sql
UPDATE proxy_session_requests
   SET day_local = COALESCE(date(started_at, 'localtime'), substr(started_at, 1, 10))
 WHERE day_local IS NULL;
```

即按**升级当时的机器时区**回填；时间戳解析失败（`date()` 返回 NULL）时退回 UTC 日期前缀，避免整行消失。同一批迁移 `0003_request_status_kind.sql` 用相同的 CASE 判定回填历史行的 `status_kind`。

## 6. 聚合与保留

`usage_daily` 是唯一的日聚合表，维度是 `(day, api_format, model, client)`，累加 `request_count`、`error_count`、四个 token、`cost_yuan`、`total_duration_ms`。概览页的五个查询（今日、本月、最近 30 天趋势、本月模型分布、最近 10 条请求）在**同一个读事务**里执行，否则并发写入时仪表盘内部数字会互相对不上。

保留策略由设置键 `retention_days` 控制，默认 90 天，允许 1–3650。清理在启动后立即跑一次，之后每 24 小时一次（`MAINTENANCE_INTERVAL`，`MissedTickBehavior::Delay`，机器休眠恢复后最多补一次）；`retention_days` 每轮从设置读取，改完下一轮生效。

| 对象 | 删除条件 | 批大小 |
| --- | --- | --- |
| `proxy_session_requests` | `day_local IS NOT NULL AND day_local < cutoff` | 每批 5000 行 |
| `proxy_request_costs` | 不单独删，靠 `ON DELETE CASCADE` 跟请求行走 | — |
| `proxy_sessions` | `last_active_at < cutoff` | 每批 5000 行 |
| `usage_daily` | `day < cutoff` | 每批 5000 行 |
| `quota_snapshots` | 先删 `status <> 'ok'` 且超过 7 天的；再按每个 `quota_type` 只留最近 200 条 | 删非 ok 单条语句 + 按 `quota_type` 循环裁剪 |

`cutoff = 今天 - (retention_days - 1)`，即保留期包含当天；`day_local IS NULL` 的异常行一律保留。有删除时最后执行一次 `ANALYZE`，否则大删除后查询计划会继续按旧估算选索引。

`exports/` 下的 CSV 不在这套清理里，保留期改小不会删掉已导出的文件。

## 7. 请求页分页：keyset

排序键是 `r.day_local DESC, r.started_at DESC, r.id DESC`。`day_local` 放在最前面才能命中 `ix_psr_day_started_id`，`id` 作为最后一级 tie-break 保证游标严格唯一。

| 项目 | 规则 |
| --- | --- |
| 游标内容 | `{ day_local, started_at, id }`，即上一页最后一行的排序键 |
| 游标谓词 | 行值比较 `(day_local, started_at, id) < (?, ?, ?)`，严格小于 |
| 与 `offset` 的关系 | 同时给出时游标优先，不会再拼 `OFFSET` |
| `total` 统计 | 与列表同事务；`include_total=false` 时跳过 `COUNT(*)` 并返回 `-1` |
| 页大小 | 前端 `PAGE_SIZE`=50；后端默认 50，钳在 1–200 |
| `offset` 上限 | 钳到 100000（深 offset 是线性扫描） |
| `next_cursor` | 只有本页满页（`rows.len() == limit`）才给出 |

并发写入下的稳定性：游标是快照语义，新请求插入到头部时不会让已翻过的页重复出现；但中途被保留期清理或手工删掉的行会形成页间空档，`total` 也可能与翻页累计条数不一致（`total` 只在第一页统计一次）。前端把游标存成栈，上一页只回退下标；收到事件刷新（新请求写入）后直接回到第一页重取。

非法筛选值（未知协议、未知状态）会返回参数错误，而不是静默退回「不过滤」；空串按未选择处理。

## 8. CSV 导出

| 项目 | 规则 |
| --- | --- |
| 数据源 | `usage_daily`（维度行），不是逐请求明细 |
| 表头 | UTF-8 BOM + `day,api_format,model,client,request_count,error_count,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_yuan` |
| 时间范围 | 只按日期区间（`from_day` / `to_day`，含端点），**不应用**协议 / 状态 / 关键字筛选 |
| 缺省区间 | 最近 30 天（含今天）；起点晚于终点直接报错 |
| 行数上限 | `MAX_EXPORT_ROWS`=200000，超过则只写到上限并标 `truncated` |
| 费用格式 | 固定 4 位小数（`{:.4}`），与界面 `round4` 一致 |
| 公式中和 | 以 `=`、`+`、`-`、`@`、制表符、回车开头的单元格前置单引号；中和后再判断是否需要加引号并把 `"` 翻倍 |
| 写盘方式 | 逐行从 SQLite 读、逐行写 `BufWriter`，内存与行数无关 |
| 文件名与落地 | `exports/usage_{from}_{to}.csv`；先写 `.csv.part` 再改名，同名旧文件先删 |

因为是流式写盘，导出中途失败不会把上一次成功的文件截成半截。

## 9. 界面单位

| 位置 | 来源 | 单位 | 格式化 |
| --- | --- | --- | --- |
| 额度卡：剩余 / 已用 / 总额 | 云端额度快照 | 积分 | `formatPoints`（最多 2 位小数）+「积分」 |
| 「本月消耗」卡片 | 云端 `current_month.usage_cost` | 积分 | 同上 |
| 「今日 Token」 | 本机 `usage_daily` | token | `formatCompact`（K / M） |
| 「今日费用」 | 本机 `usage_daily.cost_yuan` | 元 | `formatYuan`（¥ + 4 位小数） |
| 「本月 Token」 | 云端 `current_month.total_tokens` | token | `formatCompact` |
| 费用曲线 24 小时 | 云端 `/vega/user_hourly_cost` 的 `cost` | 积分 | `formatPoints` +「积分」；悬停额外给「≈ ¥」= 积分 × `credit_to_yuan` |
| 费用曲线 30 天 | 本机 `usage_daily` 汇总 | 元 | `formatYuan`（¥ + 4 位） |
| 模型分布 | 云端 `model_tokens` / `model_costs` | 积分 | `formatPoints` +「积分」；超过 8 个模型时合并为「其他 N 个模型」 |
| 请求明细的费用列 | `proxy_request_costs.cost_yuan` | 元 | `formatYuan`（¥ + 4 位） |

## 10. 已知边界（待验证）

- 请求明细的「客户端」列固定显示 `unknown`：列表查询没有关联 `proxy_sessions.client`，聚合层直接写死。
- `cost_source=estimated` 只有枚举与注释，没有写入路径；看到非 `upstream` / `none` 的值应视为数据异常。
- `QuotaInfo.currency` 后端固定序列化为 `CNY`，但界面按「积分」展示同一批字段；以上游口径为准，字段名是否该改（待验证）。
- 账号级「本月」与本地「本月」的月界在跨时区场景下必然不同，两个数字不可互相校验。
- 24 小时云端曲线只画上游返回的小时记录，没有返回的小时不会补 0（只有 30 天本机曲线会补齐缺失日期），所以曲线空档不等于该小时没有消耗。

## 源码索引

- `src-tauri/src/proxy/recorder.rs`、`src-tauri/src/proxy/usage_parser.rs`
- `src-tauri/src/proxy/routes/context.rs`、`src-tauri/src/proxy/routes/forward.rs`、`src-tauri/src/proxy/routes/stream_proxy.rs`
- `src-tauri/src/proxy/forward.rs`、`src-tauri/src/proxy/policy.rs`
- `src-tauri/src/usage/mod.rs`、`src-tauri/src/usage/aggregate.rs`、`src-tauri/src/usage/queries.rs`
- `src-tauri/src/commands/usage.rs`
- `src-tauri/src/db/repo.rs`、`src-tauri/src/db/sql.rs`、`src-tauri/src/db/maintenance.rs`
- `src-tauri/migrations/0003_request_day_status.sql`、`src-tauri/migrations/0003_request_status_kind.sql`
- `src-tauri/src/bootstrap.rs`、`src-tauri/src/settings.rs`
- `src/pages/Requests.tsx`、`src/pages/dashboard/index.tsx`、`src/pages/dashboard/CostCurve.tsx`
- `src/lib/types.ts`、`src/lib/format.ts`、`src/lib/quota.ts`、`src/lib/labels.ts`
- `scripts/check-public-repo.mjs`
