# 兼容网关契约

> 这篇是本应用**依赖**的兼容网关契约：登录、校验、续期、额度这些端点长什么样，上游模型路径怎么映射，用量元数据从哪儿取；给要自己实现一个兼容网关的人看，也给怀疑「是网关字段变了还是客户端坏了」的排障者看。

## 这份契约的定位

- 这是**客户端契约**，不是上游实现的完整文档：它描述「本应用会怎么发请求、怎么读响应」，兼容网关只要满足这些行为，应用就能工作。
- 事实来源是本仓库的代码与测试 fixture，**不引用任何真实部署地址**。文中出现的 `https://api.example.test` 一类地址都是 fixture 里的占位值，默认网关在设置里是 `https://api.example.com`，由用户自己替换。
- 网关地址在「设置 → 代理与记录」里填写（键名 `gateway_url`）：`https://` 任意主机都接受；`http://` 只接受本机回环（`127.0.0.1` / `localhost` / `::1`），因为凭据是以请求头形式发过去的，明文链路会把 token 暴露在同网段。
- 所有认证请求统一 20 秒超时、显式**不跟随重定向**（`X-Access-Token` 是自定义头，跨主机重定向不会被标准库剥离，跟随就会泄给 `Location` 主机），UA 为 `codemaker-hub-light/<版本>`。

## 登录

### 发起登录

| 项 | 值 |
| --- | --- |
| 请求 | `GET {gateway}/vs_login?ide=<ide>&redirect_uri=<percent-encoded>` |
| `ide` | 固定 `codemaker-hub-light` |
| `redirect_uri` | `codemakerhub-light://auth/callback?pollUuid=<uuid>`（百分号编码后拼进 query） |
| 谁发起 | 客户端只负责拼 URL 并交给系统浏览器；应用自己**不请求**这个端点 |

浏览器侧完成后有两条回传通道，任一即可，应用两条都实现：

1. **深链回跳**：`codemakerhub-light://auth/callback?pollUuid=<uuid>?access_token=..&access_sso_token=..&access_user=..`
   注意分隔符是**第二个 `?`**（不是 `&`），客户端按 `?` 与 `&` 同时切分；`pollUuid` 必须是合法 UUID。
   参数别名：`pollUuid`/`poll_uuid`/`polluuid`；`access_token`/`accessToken`/`token`；`access_sso_token`/`auth_key`/`authkey`/`sso_token`；`access_user`/`user`/`auth_user`。
2. **轮询**：见下一节。深链没送到时靠它兜底，两者可以同时到达。

### 轮询登录状态

| 项 | 值 |
| --- | --- |
| 请求 | `GET {gateway}/vs_login_info/{pollUuid}` |
| 鉴权 | 无（此时还没有 token） |
| 未完成 | `200` + `{}`（或任何不含 token 的 JSON） |
| 明确失败 | `200` 且 `credentials_invalid`（或 `credentialsInvalid`）为 `true` |
| 被拒绝 | `401` / `403` → 客户端提示「服务端拒绝该登录请求，请重新发起登录」并结束本次登录 |

成功响应的字段与别名（客户端全部容忍，取到第一个非空值即用）：

| 语义 | 字段（按优先级） | 必填 | 说明 |
| --- | --- | --- | --- |
| 访问令牌 | `access_token`、`accessToken`、`token` | 是 | 字符串，去首尾空格；缺失即视为「未完成」 |
| 续期 key | `auth_key`、`authkey`、`access_sso_token`、`sso_token` | 否 | 后续还会被 `/get_auth_key` 覆盖 |
| 用户标识 | `user`、`access_user`、`username`、`employee_id` | 否 | 缺失时回退 `/validate` 的 `display_name`，两者都没有则登录失败 |
| 网关地址 | `serverUrl`、`server_url`、`server` | 否 | 只接受 `http(s)`，否则回退设置里的 `gateway_url` |
| 过期时间 | `expire`、`expires_at`、`expiresAt`、`expire_at` | 否 | epoch 秒；缺失时按 86400 秒兜底 |

响应可以被 `{"code":0,"data":{...}}` 包裹一层：外层有 `data` 且是对象时只读内层。

客户端节奏：每 2 秒轮询一次，总超时 300 秒；连续 3 次网络错误转失败；`200` 但没拿到 token 时继续轮询、不计错误。

### `POST /api/v2/tokens`（账号 + AuthKey 直登）

不经过浏览器/深链的单次 RPC：用账号名与 AuthKey 直接换 token。

| 项 | 值 |
| --- | --- |
| 请求 | `POST {gateway}/api/v2/tokens`，头 `Content-Type: application/json`、`Accept: application/json` |
| 请求体 | `{"user": "<账号名>", "auth_key": "<AuthKey>"}` |
| 成功 | `200` + JSON（字段见下表） |
| 账号/AuthKey 错误 | `401` / `403` → 客户端提示「账号名或 AuthKey 不正确。」 |
| 其它 4xx | 客户端提示「登录请求被服务端拒绝（HTTP n）。」 |
| `5xx` / `429` / 网络错误 | 按网络错误处理；不改变已有登录态 |

成功响应字段（客户端容忍别名与 `{"data": {...}}` 包裹，取第一个非空值）：

| 语义 | 字段（按优先级） | 必填 | 说明 |
| --- | --- | --- | --- |
| 访问令牌 | `access_token`、`accessToken`、`token` | 是 | 缺失/为空视为失败（不会把畸形响应当成登录成功） |
| 续期 key | `auth_key`、`authkey`、`access_sso_token`、`sso_token` | 否 | 登录后仍会优先用 `/get_auth_key` 的 128 字符 key；取不到才回退这里或本次输入的 AuthKey |
| 用户标识 | `access_user`、`user`、`username`、`employee_id` | 否 | 缺失时回退本次输入的账号名 |
| 网关地址 | `server_url`、`serverUrl`、`server` | 否 | 缺失时回退设置里的 `gateway_url` |
| 过期时间 | `expire`、`expires_at`、`expiresAt`、`expire_at` | 否 | epoch 秒；缺失时按 86400 秒兜底 |

该命令成功后仍会走与浏览器登录相同的收尾：`/validate` → 模型目录（`/validate` 未带时补 `/validate/models`）→ `/get_auth_key` → 额度；因此网关需要同时实现上面的端点。

### `GET /validate`

| 项 | 值 |
| --- | --- |
| 请求 | `GET {gateway}/validate`，头 `X-Access-Token: <access_token>`、`Accept: application/json` |
| 成功 | `2xx` + JSON（同样支持 `data` 包裹） |
| 凭据失效 | `401` / `403` → 转「需要重新登录」 |
| 其它 | 非 2xx 上抛；网络错误降级为**离线宽限**（界面上仍算已登录，但不再刷新校验时间）；`200` + 畸形 JSON 视为解码失败，**不能**当成通过 |

| 字段 | 别名 | 用途 |
| --- | --- | --- |
| `dept` | `department` | 账号档案里的部门 |
| `display_name` | `displayName`、`name` | 展示名 |
| `gateway` | — | 网关地址（优先级低于 `server_url`） |
| `server_url` | `serverUrl`、`server` | 后续转发优先使用的地址 |
| `models` | — | 模型目录（形状见下）；**非空时客户端不再请求** `/validate/models` |

### `GET /validate/models`

| 项 | 值 |
| --- | --- |
| 请求 | `GET {gateway}/validate/models`，头 `X-Access-Token` |
| 容忍 | `401` / `403` / `404` → 返回空目录，**不阻断登录** |
| 其它非 2xx | 上抛（`5xx`、`429` 不能被当成「没有值」） |

目录形状（客户端支持三种）：

```text
["gpt-4o"]
{ "models": [{ "id": "claude-sonnet-4", "display_name": "Claude Sonnet 4" }] }
{ "code": 0, "data": { "completion_langs": [{ "model": "x", "name": "X" }] } }
```

规则：顶层数组，或 `models` / `model` / `model_list` / `completion_langs` 数组；元素为字符串时就是 id；元素为对象时取 `id` / `model` / `name` 之一作为 id，取 `display_name` / `displayName` / `label`（或与 id 不同的 `name`）作为展示名。

### `GET /get_auth_key`

| 项 | 值 |
| --- | --- |
| 请求 | `GET {gateway}/get_auth_key`，头 `X-Access-Token` |
| 成功 | `2xx` + `{"auth_key": "..."}`（别名 `authkey`、`sso_token`，同样支持 `data` 包裹） |
| 容忍 | `401` / `403` / `404` → 视为「本次拿不到 key」，登录继续，只影响自动续期 |

语义要点：这里返回的是**续期 key**（代码注释记录为 128 字符），与深链里的 `access_sso_token`（代码注释记录为 225 字符）不是同一个东西。拿 `/get_auth_key` 的 key 去调 `/renew_token` 才能成功；用深链那个会被服务端以 `403` + `decrypt auth key fail` 拒掉。客户端每轮续期前都会重新取一次（key 可能轮换），取不到才回退已存值。

### `POST /renew_token`

| 项 | 值 |
| --- | --- |
| 请求 | `POST {gateway}/renew_token`，头 `Content-Type: application/json`、`Accept: application/json` |
| 请求体 | `{"key": "<续期 key>", "user": "<用户标识>"}` |
| 成功 | `2xx` + `{"token": "...", "user": "...", "ttl": "...", "expire": <epoch 秒>}` |
| 必读字段 | token：`access_token` / `accessToken` / `token`；缺失即解码失败 |
| 过期时间 | `expire` / `expires_at` / `expiresAt` / `expire_at`（可数字或数字字符串） |
| 拒绝 | `401` / `403` → 需要重新登录 |

约束（都是踩过的坑）：

- 只认**JSON 请求体**。把 key 放进鉴权头、或者发 `{"auth_key": ...}`，服务端一律 `422`。
- `403` 是终态：客户端单次失败即返回，不重试；只有「主 key 与已存 key 不同」时才用已存 key 再试一次。
- 续期必须真的延长有效期。反复拿到同一个 `expire` 会被判为「无进展」，连续 3 次即要求重新登录。

客户端节奏常量：每 60 秒检查一次；`expire - now <= 3600` 或距上次成功 ≥ 82800 秒时才续期；成功后续期静默 600 秒；失败退避 60 → 120 → 240 → 300 秒封顶。

## 额度

| 项 | 值 |
| --- | --- |
| 请求 | `GET {gateway}/api/v1/user_quota/usage?quota_type=global`，头 `X-Access-Token` |
| `200` | 按下面三套形状解析，原始 JSON 会整份存进本地 `quota_snapshots.raw_json` 便于字段改名时排查 |
| `401` / `403` | 视为无权限：不覆盖上次快照 |
| `404` | 视为接口不存在：同样保留上次快照 |
| 其它 | 上抛为请求失败 |

兼容三种返回形态（真实字段名有多种写法，客户端做的是「别名 + 来源分层」匹配）：

| 语义 | 顶层字段（优先） | 回退来源 |
| --- | --- | --- |
| `monthly_quota` | `monthly_quota`、`monthlyQuota`、`monthly_total`、`monthlyTotal`、`total`、`quota` | `quota_config.monthly_quota`、`current_month.monthly_quota` |
| `remaining` | `remaining`、`remain`、`remaining_quota`、`remainingQuota`、`left` | `current_month.remaining` / `remain` |
| `used` | `used`、`usage_cost`、`usageCost`、`used_quota`、`usedQuota` | `usage_cost`；再退 `monthly_quota - remaining`（不小于 0） |
| `usage_cost` | `usage_cost`、`usageCost`、`cost` | `current_month.usage_cost` |
| `percentage` | `percentage`、`percent`、`used_percentage`、`usedPercentage` | `current_month.percentage` / `percent` |
| `reset_day` | `reset_day`、`resetDay` | `quota_config.reset_day` |
| `is_active` | `is_active`、`isActive`、`active` | `quota_config.is_active` |
| `period_start` | `period_start`、`periodStart`、`start`、`period_start_at` | 由 `current_month.year` + `month` 拼成 `YYYY-MM-01` |
| `reset_at` | `reset_at`、`resetAt`、`reset_time`、`resetTime` | — |
| `timezone` | `timezone`、`tz` | — |
| 月度分布 | `current_month.total_tokens`、`app_tokens`、`app_costs`、`model_tokens`、`model_costs`（均有 camelCase 别名） | — |

约定：**缺字段就留 `null`，不要伪造 0**。客户端把「没有值」和「值为零」分开处理，填 0 会让界面显示「额度已用尽」。`currency` 由客户端固定标注 `CNY`，`status` 由客户端固定写 `ok`。

嵌套形状（对应 fixture `quota_nested.json`）：

```json
{
  "quota_config": { "is_active": true, "monthly_quota": 3500.04, "reset_day": 1 },
  "current_month": { "usage_cost": 58.04, "remaining": 3442.0, "percentage": 1.66,
                     "year": 2026, "month": 10, "total_tokens": 802378086,
                     "app_tokens": { "_codemaker_cli": 802364696 } },
  "last_month": { "month": 9, "year": 2026, "usage_cost": 12.5 }
}
```

## 上游模型路径映射

转发时 URL 拼法是「网关去尾 `/` + 固定上游路径 + 客户端原始 query」。客户端只暴露三条协议入口：

| 客户端路径（含无 `/v1` 别名） | 上游路径 | 协议 | 是否本地翻译 |
| --- | --- | --- | --- |
| `POST /v1/messages`、`/messages` | `/openai/v1/chat/completions` | Anthropic Messages → OpenAI Chat | 是（请求体与响应体双向翻译） |
| `POST /v1/chat/completions`、`/chat/completions` | `/openai/v1/chat/completions` | OpenAI Chat | 否，同协议直通 |
| `POST /v1/responses`、`/responses` | `/openai/v1/responses` | OpenAI Responses | 否，同协议直通 |

因此兼容网关最少要实现两个端点：`POST /openai/v1/chat/completions` 与 `POST /openai/v1/responses`。**没有** `/anthropic/...` 这类上游路径——Anthropic 请求是在客户端本地翻译成 OpenAI Chat 之后才发出去的。

网关地址的取值顺序：登录/校验拿到的 `serverUrl`（只接受 `http://` / `https://` 开头）优先，否则用设置里的 `gateway_url`。

## 用量元数据

上游可以在响应体里夹带费用与链路信息，客户端按下面的位置读：

| 语义 | 首选位置 | 兼容位置 | 类型 |
| --- | --- | --- | --- |
| `aigw.cost.total_credit` | `payload.aigw.cost.total_credit` | `payload.usage.aigw.cost.total_credit` | 数字，或数字字符串（两种都收） |
| `aigw.trace_id` | `payload.aigw.trace_id` | `payload.usage.aigw.trace_id` | 仅字符串 |

- 出现时机：SSE 场景通常在末帧（与 `usage` 同一帧），非流式场景在响应体里；`aigw` 既可能与 `usage` 同级，也可能被包在 `usage` 下。
- 换算：`cost_yuan = total_credit × credit_to_yuan`，`credit_to_yuan` 默认 `0.01`（即 `total_credit / 100`），可在设置里改；结果保留 8 位小数。
- 样本：`total_credit = 2.843328` → `0.02843328` 元；`0.811536` → `0.00811536` 元。
- 拿不到 `aigw.cost` 时费用来源记为 `none`、金额为 `0`，不会瞎估。
- token 四元组归一：OpenAI 系 `prompt_tokens` 含缓存命中，客户端做减法 `input = prompt_tokens - cached`；缓存读/写分别取 `prompt_tokens_details.cached_tokens` / `cache_write_tokens`（OpenAI Chat）、`input_tokens_details.*`（Responses）、`cache_read_input_tokens` / `cache_creation_input_tokens`（Anthropic）。

## 未登录占位值

上游在「没有登录」时会返回三个占位字符串，客户端把它们**等同为空值**：

| 占位值 | 位置 |
| --- | --- |
| `unknown_access_token` | 登录响应 / 深链的 `access_token` |
| `unknown_sso_token` | 深链的 `access_sso_token` |
| `unknown_auth_user` | 深链的 `access_user` |

语义：出现了不等于「拿到了一个坏 token」，而是「服务端明说没有」。客户端据此按未登录处理——不写入登录态、不注入到上游请求头、不因此把状态翻成错误。除占位值外，超过 8192 字符（用户标识 256 字符）或含控制字符的凭据字段也一律丢弃。

## 错误与状态码约定

| 状态 | 客户端解释 | 行为 |
| --- | --- | --- |
| `401` / `403` | 凭据失效或无权限 | `/validate` → 转「需要重新登录」；`/renew_token` → 同上（仅在换用已存 key 时才重试一次）；`/get_auth_key`、`/validate/models` → 容忍为「本次没有」；额度接口 → 保留上次快照 |
| `404` | 端点不存在 | `/get_auth_key`、`/validate/models` 容忍为空；额度接口保留上次快照并提示「接口不存在，请检查服务版本」 |
| `5xx` / `429` | 真故障 | 上抛，**不能**被当成「没有值」，否则一次网关故障会被静默降级成「取不到 key」并继续用旧凭据 |
| `200` + 畸形 JSON | 解码失败 | 上抛，不能记成成功 |
| 网络错误 | 不可达 / 超时 | 登录期连续 3 次转失败；`/validate` 期间转离线宽限 |
| `200` + 业务错误 | 部分端点把业务错误放在正文 `error` 字段（如 `/vega/user_hourly_cost` 的时间范围超限） | 按失败处理，message 透给用户 |

## 端点总表

| 方法 | 路径 | 用途 | 鉴权 | 关键字段 |
| --- | --- | --- | --- | --- |
| `GET` | `/vs_login?ide=&redirect_uri=` | 打开登录页 | 无 | `ide`、`redirect_uri` |
| `GET` | `/vs_login_info/{pollUuid}` | 轮询登录结果 | 无 | `access_token`、`auth_key`、`user`、`serverUrl`、`expire`、`credentials_invalid` |
| `POST` | `/api/v2/tokens` | 账号 + AuthKey 直登 | JSON body `{user,auth_key}` | `access_token`、`auth_key`、`access_user`、`server_url`、`expire` |
| `GET` | `/validate` | 校验凭据 + 取账号档案 | `X-Access-Token` | `dept`、`display_name`、`gateway`、`server_url`、`models` |
| `GET` | `/validate/models` | 模型目录 | `X-Access-Token` | `models` / `model` / `model_list` / `completion_langs` |
| `GET` | `/get_auth_key` | 取续期 key | `X-Access-Token` | `auth_key` |
| `POST` | `/renew_token` | 续期访问令牌 | JSON body `{key,user}` | 请求 `key`/`user`；响应 `token`/`expire` |
| `GET` | `/api/v1/user_quota/usage?quota_type=global` | 月度额度 | `X-Access-Token` | `quota_config`、`current_month`、`monthly_quota`、`remaining` |
| `GET` | `/vega/user_hourly_cost?start_time=&end_time=` | 账号小时费用（可选） | `X-Access-Token` | `records[]`：`snapshot_hour`、`cost`、`total_cost`、`app_tokens` |
| `POST` | `/openai/v1/chat/completions` | 上游模型调用 | `x-access-token` + `Authorization: Bearer` + `x-auth-user` | OpenAI Chat 全字段 + `usage` + `aigw` |
| `POST` | `/openai/v1/responses` | 上游模型调用 | 同上 | OpenAI Responses 全字段 + `usage` + `aigw` |

## 兼容网关最小实现清单

1. `GET /vs_login` 跳转登录页，完成后通过深链回跳或让 `/vs_login_info/{uuid}` 返回 token。
2. 若需要账号 + AuthKey 直登：`POST /api/v2/tokens` 接受 `{user,auth_key}` 并返回 `access_token`（可选 `auth_key`/`access_user`/`server_url`/`expire`）。
3. `GET /validate` 回 `dept` / `display_name` / `server_url`，并尽量在 `models` 里带上模型目录。
4. `GET /get_auth_key` 回 `auth_key`；`POST /renew_token` 接受 `{key,user}` 并返回新的 `token` 与更晚的 `expire`。
5. `GET /api/v1/user_quota/usage?quota_type=global` 至少给出 `monthly_quota`、`remaining`、`is_active` 之一。
6. `POST /openai/v1/chat/completions` 与 `POST /openai/v1/responses` 接受 `X-Access-Token` 凭据，并在 `usage` 或 `aigw` 里回 `total_credit` / `trace_id`（可选但强烈建议）。

## 源码索引

- `src-tauri/src/auth/mod.rs`
- `src-tauri/src/auth/client.rs`
- `src-tauri/src/auth/quota.rs`
- `src-tauri/src/auth/hourly.rs`
- `src-tauri/src/auth/service/login.rs`
- `src-tauri/src/auth/service/validate.rs`
- `src-tauri/src/auth/service/renewal.rs`
- `src-tauri/src/auth/service/mod.rs`
- `src-tauri/src/proxy/forward.rs`
- `src-tauri/src/proxy/usage_parser.rs`
- `src-tauri/src/settings.rs`
- `src-tauri/tests/fixtures/auth/`：`login_success.json`、`login_success_aliases.json`、`login_invalid.json`、`login_pending.json`、`validate.json`、`validate_wrapped.json`、`models.json`、`auth_key.json`、`renew.json`、`renew_aliases.json`、`quota.json`、`quota_camel.json`、`quota_nested.json`
