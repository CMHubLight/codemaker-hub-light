# 本地代理

> 这篇讲 CodeMaker Hub Light 内置的本地代理：它怎么起、监听哪个端口、对外暴露哪些路径、怎么改写鉴权头、怎么在本地翻译 Anthropic 协议、以及各种上限与错误码；给接入这个代理的客户端作者和排障的人看。

## 这个代理是什么

代理是 Tauri 进程内的一块 Axum 服务，只监听回环地址，把三类协议（Anthropic Messages、OpenAI Chat、OpenAI Responses）转发到设置里填写的兼容网关，同时给每个请求记一行用量。它的边界很窄：

- 不做重试、不做多上游负载均衡；
- 不跟随上游重定向（`reqwest` 显式设为 `Policy::none()`）；
- 除 `/v1/messages` 的翻译路由外，不改写请求体与响应体；
- token 只在内存里用于注入上游请求头，不落日志、不落库。

## 启动、绑定与端口回退

代理由 `bootstrap::spawn_proxy` 在 Tauri 的 Tokio runtime 里异步启动。启动失败只记日志并发 `proxy://error` 事件，应用继续运行，之后可以调 `proxy_restart` 重试。

| 项 | 行为 | 依据 |
| --- | --- | --- |
| 绑定地址 | 设置项 `proxy_bind`，默认 `127.0.0.1`；只接受 `127.0.0.1` / `localhost` / `::1`，其它值启动时直接报 `NonLoopback` | `proxy/server.rs`、`settings.rs` |
| 首选端口 | 设置项 `proxy_port`，默认 `15721`，校验要求 ≥ 1024 | `settings.rs` |
| 回退区间 | 首选端口被占用时依次尝试 `首选+1 ..= 15800`（`PROXY_PORT_FALLBACK_END`） | `proxy/server.rs` |
| 最终兜底 | 区间内全失败则向系统要一个临时端口（`port = 0`） | `proxy/server.rs` |
| 首选端口 > 15800 | 不再回退区间，只用首选端口，再退到系统分配 | `proxy/server.rs` |
| 端口文件 | `~/.codemaker-hub-light/proxy.port`，内容是**实际**监听端口的纯文本，供外部 CLI 发现 | `settings::write_proxy_port` |
| 停止 | 发 oneshot 触发 graceful shutdown，最多等 5 秒（`STOP_TIMEOUT`），幂等 | `proxy/server.rs` |

`proxy.port` 与 `settings.json` 一样走「临时文件 + `fsync` + `rename`」的原子写；写失败只记 warn，不影响代理继续服务。

代理启动成功后状态经 `proxy://status` 事件推给前端，字段为 `running` / `host` / `port` / `base_url` / `gateway_url` / `last_error` / `started_at` / `uptime_secs` / `request_count` / `error_count` / `port_file` / `health_url`。

## 路由表

`routes::router` 注册的全部路径如下（清单块由门禁脚本与代码逐项对账）：

```text name=proxy-routes
/health
/status
/v1/models
/models
/internal/quota
/v1/messages
/messages
/v1/messages/count_tokens
/messages/count_tokens
/v1/chat/completions
/chat/completions
/v1/responses
/responses
```

| 方法 | 路径 | 处理器 | 说明 |
| --- | --- | --- | --- |
| `GET` | `/health` | `diagnostics::health` | 存活探测 |
| `GET` | `/status` | `diagnostics::status` | 运行状态快照 |
| `GET` | `/v1/models`、`/models` | `diagnostics::models` | 模型目录（OpenAI 形状） |
| `GET` | `/internal/quota` | `diagnostics::internal_quota` | 最近一次成功额度快照（只读本地库） |
| `POST` | `/v1/messages`、`/messages` | `forward::forward_handler` | 翻译为 OpenAI Chat 后转发 |
| `POST` | `/v1/messages/count_tokens`、`/messages/count_tokens` | `diagnostics::count_tokens` | 本地估算，不访问上游 |
| `POST` | `/v1/chat/completions`、`/chat/completions` | `forward::forward_handler` | OpenAI Chat 直通 |
| `POST` | `/v1/responses`、`/responses` | `forward::forward_handler` | OpenAI Responses 直通 |
| 任意 | 其它路径 | `diagnostics::not_found` | 兜底 404 |

带 `/v1` 与不带 `/v1` 是**等价的两条注册路由**，不是重写规则：`/messages`、`/chat/completions`、`/responses`、`/messages/count_tokens` 分别与带前缀版本指向同一处理器。转发层另有一份别名表 `forward::ROUTE_ALIASES`，只服务三条转发路由的规范化（`/messages` → `/v1/messages` 等），查询串在规范化时被忽略、但在拼上游 URL 时原样带上。

## 鉴权头与转发头

上游请求头由三段拼成：复制客户端业务头 → 剥离敏感头 → 注入本地凭据。

**剥离**（客户端自带的凭据、浏览器上下文、hop-by-hop）：

| 类别 | 头 |
| --- | --- |
| 凭据类 | `authorization`、`x-api-key`、`api-key`、`x-goog-api-key`、`x-access-token`、`cookie` |
| 浏览器/连接类 | `origin`、`host`、`content-length` |
| hop-by-hop | `connection`、`keep-alive`、`proxy-authenticate`、`proxy-authorization`、`te`、`trailer`、`transfer-encoding`、`upgrade` |

**注入**（值来自登录态快照，空值跳过）：

| 头 | 值 | 说明 |
| --- | --- | --- |
| `x-access-token` | `<access_token>` | 主凭据 |
| `authorization` | `Bearer <access_token>` | 部分上游只认标准头，因此一并注入 |
| `x-auth-user` | `<user>` | 账号标识 |

代理**不**向上游注入自身标识头（代码里明确不注入 `downstream-client`，因为部分网关会据此判定为非直连客户端并拒绝）。

翻译路由（`/v1/messages*`）在此之上再改一次：移除 `anthropic-version`、`anthropic-beta`、`x-api-key`、`content-length`，并强制 `content-type: application/json`、`accept: text/event-stream`（流式）或 `application/json`、`accept-encoding: identity`。

响应头只去 hop-by-hop，其余（含 `content-type`、`content-encoding`）原样透传；翻译路由会额外去掉 `content-length` 与 `content-encoding` 并重设 `content-type`。所有转发响应都会 `append` 一个 `x-hub-request-id`（用 `append` 而非 `insert`，上游若回显同名头则保留多值）。

落库的请求头走白名单，绝不含凭据：`user-agent`、`content-type`、`accept`、`anthropic-version`、`anthropic-beta`、`openai-beta`、`x-client-request-id`、`x-claude-code-session-id`、`x-codemaker-session-id`、`x-session-id`、`x-opencode-session-id`、`downstream-client`、`x-stainless-*`。

## Anthropic Messages → OpenAI Chat 的翻译边界

只有 `POST /v1/messages`（含 `/messages`）会翻译，上游固定走 `/openai/v1/chat/completions`。翻译是单趟解析，同一次解析顺带产出 `model`、`stream` 与输入 token 估算。

| Anthropic 字段 | 处理 | 备注 |
| --- | --- | --- |
| `model` | 必填，缺失即 400 | 空字符串也算缺失 |
| `messages` | 必填、必须非空数组 | 每项 `content` 必须是字符串或数组，否则 400 |
| `system` | 字符串或 `text` 块数组 → 首条 `role: system` 消息 | 数组里非 `text` 块被忽略 |
| `max_tokens`、`temperature`、`top_p` | 仅当是数字时原样复制 | 其它类型静默丢弃 |
| `stop_sequences` | 转 `stop`，仅当非空数组 | |
| `stream` | 原样复制；为 `true` 时补 `stream_options: {include_usage: true}` | 用来让上游末帧带 usage |
| `tools` | `name` / `description` / `input_schema` → `function` 的 `name` / `description` / `parameters` | `input_schema` 缺省时补 `{"type":"object","properties":{}}` |
| `tool_choice` | `auto`→`auto`、`any`→`required`、`none`→`none`、`tool`→指定 `function` | 只在请求里存在 `tools` 时才下发，否则丢弃 |

| 内容块 | 处理 |
| --- | --- |
| assistant `text` | 多块拼成一个字符串 |
| assistant `tool_use` | 转 `tool_calls[]`，`input` 序列化成 `arguments` 字符串；缺 `id` 时生成 `toolu_<uuid>` |
| assistant `thinking` / `redacted_thinking` / 未知块 | 丢弃 |
| user `text` | 转 `text` 块 |
| user `image` | `source.type=base64` 拼成 `data:<media_type>;base64,<data>`；`source.type=url` 直取 URL；转 `image_url` |
| user `tool_result` | 转独立的 `role: tool` 消息；`is_error: true` 时正文前缀 `[tool_error] ` |
| user `document` | 只在 `source.type=text` 时转成 `text` 块，其它形态丢弃 |
| 顶层 `metadata` / `top_k` / `thinking` / `cache_control` | 整段丢弃：首版不保留 thinking 与提示缓存 |

`count_tokens` 完全本地估算，不访问上游（避免 Claude Code 因 404 中断）。口径是「先数字符，再除以 4，最小 1」：

| 计入项 | 算法 |
| --- | --- |
| `system` | 字符串字符数，或数组中每块 `text` 的字符数 |
| 消息 `content` 字符串 | 全部字符数 |
| `text` 块 | `text` 字段字符数 |
| `image` 块 | 固定 +4000 字符（约等于 1000 token） |
| `tool_use` 块 | `input` 的 JSON 序列化字符数 |
| `tool_result` 块 | 递归其 `content` |
| `tools` | `description` 字符数 + `input_schema` 的 JSON 序列化字符数 |

非流式响应翻译：`choices[0].message.content` → `text` 块，`tool_calls` → `tool_use` 块（`arguments` 解析失败回 `{}`），`finish_reason` 映射为 `length→max_tokens`、`tool_calls`/`function_call→tool_use`、其余 `end_turn`，`usage` 映射为 `input_tokens`/`output_tokens`/`cache_*`。流式响应由 `StreamTranslator` 状态机把 OpenAI SSE 增量翻译成 Anthropic SSE：`message_start` 里的输入 token 用本地估算值，文本块延迟到首个 delta 才开块，工具参数先按 OpenAI `index` 聚合、结束时按 index 顺序整块输出（避免并行工具造成块交错）。客户端要流式但上游回了 JSON 时，会把该 JSON 合成一次性 SSE；上游返回 `error` 对象、流为空或单帧超过 2 MiB 时，都改为一条 `event: error`。

翻译失败统一返回 400 `invalid_request_error`，message 为具体原因（不是合法 JSON / 请求体必须是 JSON 对象 / 缺少 model / 缺少 messages 数组 / messages 不能为空 / content 类型错误）。

## SSE 转发与用量记录

响应体不用整段缓冲：`mpsc` 通道（容量 16）配 `Body::from_stream`，边收边发。

- **流式**：每个 chunk 先喂给 `SseTee` 做用量观测，再原样透传（翻译路由则喂给状态机）。帧分隔兼容 `\n\n` 与 `\r\n\r\n`，忽略 `:` 注释行，支持多行 `data:`；`[DONE]` 与心跳不影响 usage。
- **非流式**：最多缓冲 8 MiB 用于解析 usage；一旦超限就清空缓冲、标记 `response_truncated`，之后只透传不解析。
- **两个 2 MiB 上限语义不同**：观测侧 `SSE_PARSE_LIMIT` 只是停止解析（字节照常透传）；翻译侧 `SSE_FAIL_LIMIT` 会清空缓冲并给客户端发一条 `event: error`。
- **客户端断开**：靠通道 `send` 失败感知，记为 `error_kind = client_aborted`。
- **落库**：单事务写 `proxy_sessions`（按会话 upsert）、`proxy_session_requests`、`proxy_request_costs`、`usage_daily`；写失败只记日志，不影响已经发出去的响应。`error_kind` 文本只有一份来源：`timeout`、`upstream_error`、`upstream_redirect`、`client_aborted`、`translation_error`、`unauthorized`、`request_body`。
- **首字节延迟**：流式响应在收到首个上游 chunk 时记录 `first_byte_ms`（从请求开始计），落库为 `proxy_session_requests.first_byte_ms`；非流式请求保持 `NULL`。口径是"首个上游响应字节"，不是"首个 token"，界面列名与文档都按此口径表述。
- **早期失败也落库**：401 / 413 / 400 / 504 这些没走到上游响应的请求同样记一行，并计入 `error_count`，否则会出现「有错误数、没有请求数」的矛盾。

## 上限与超时

| 项 | 值 | 常量 | 能否由设置改 |
| --- | --- | --- | --- |
| 请求体硬上限 | 32 MiB | `DEFAULT_MAX_REQUEST_BODY_BYTES` | 否 |
| 单侧正文记录上限 | 64 KiB | `DEFAULT_MAX_BODY_BYTES` | 否 |
| 非流式解析缓冲 | 8 MiB | `DEFAULT_MAX_BUFFER_BYTES` | 否 |
| SSE 观测解析缓冲 | 2 MiB | `SSE_PARSE_LIMIT` | 否 |
| SSE 翻译缓冲 | 2 MiB | `SSE_FAIL_LIMIT` | 否 |
| 上游连接超时 | 10 s | `CONNECT_TIMEOUT_SECS` | 否 |
| 上游首字节超时 | 120 s | `FIRST_BYTE_TIMEOUT_SECS` | 仅在策略结构体里，设置页未暴露 |
| 上游流空闲超时 | 300 s（相邻 chunk 间隔） | `IDLE_TIMEOUT_SECS` | 否 |
| 是否记录请求/响应正文 | 默认关闭 | `ProxyPolicy::record_body` | 是（设置项 `record_body`） |
| `total_credit` 换算为元的系数 | 0.01 | `credit_to_yuan` | 是（设置项 `credit_to_yuan`） |

要点：

- 只有 `record_body` 与 `credit_to_yuan` 两项来自设置。`ProxyPolicy::from_settings` 对其余字段一律取 `Default`，所以工程上要改上限得改常量。
- 策略是「每请求取一次快照」的 `PolicyCell`：改完设置下一请求即生效，不用重启代理。
- 记录正文按 UTF-8 字符边界截断，不会切出半个汉字。
- 413 的提示文案固定写 `32MB`（`REQUEST_BODY_LIMIT_HINT`），实际拒绝按 `max_request_body_bytes` 字节数判断，两者理论上可漂移。

## 状态码与错误体

本地生成三类错误形状：Anthropic 形状 `{"type":"error","error":{"type":...,"message":...}}`、OpenAI 形状 `{"error":{"message":...,"type":...,"code":...}}`、诊断端点形状 `{"error":<code>,"message":...}`。

| 状态 | 本地 code | 触发条件 |
| --- | --- | --- |
| 400 | `invalid_request_error` | Anthropic 请求翻译失败；`count_tokens` 请求体不是合法 JSON |
| 401 | `authentication_error` | 没有可用凭据（未登录 / 已失效） |
| 403 | `origin_not_allowed` | 请求带 `Origin` 头 |
| 403 | `host_not_allowed` | `Host` 不是回环 |
| 404 | `not_found` | 路径不在路由表 |
| 413 | `request_too_large` | 请求体超过上限 |
| 502 | `upstream_unavailable` | 连不上上游（连接失败、DNS 等） |
| 502 | `upstream_redirect` | 上游返回 3xx，代理拒绝跟随 |
| 503 | `service_unavailable` | `/v1/models` 的模型目录为空 |
| 504 | `upstream_timeout` | 上游请求超时（文案「上游响应超时」）或首字节超时（文案「上游首字节超时」） |
| 500 | `db_unavailable` / `quota_read_failed` | `/internal/quota` 读库失败 |

上游的 4xx / 5xx 不转换：状态码与响应体原样透传给客户端（翻译路由会把错误体转成 Anthropic 形状）。重定向是唯一例外——`3xx` 一律换成 502，因为注入的凭据是自定义头，`reqwest` 跨主机重定向不会剥离它，跟随就会把凭据带向非预期主机。

## 入口守卫：Origin 与 Host

两道中间件覆盖所有路由：

- **Origin**：请求头里只要出现 `Origin` 就直接 403 `origin_not_allowed`。浏览器发起的 `fetch`/`EventSource` 一定带这个头，命令行与 SDK 客户端一般不带——这条规则的作用是把「网页偷偷调用本地代理」整个堵掉。
- **Host**：`Host` 必须是 `127.0.0.1`、`localhost`、`::1` 或实际绑定地址之一，大小写不敏感，允许 `localhost.` 这种带尾点的 FQDN 写法，IPv6 必须带方括号；缺头、语法非法、端口为 0 或越界一律 403 `host_not_allowed`。这条防的是 DNS rebinding：页面把自己的域名解析到 `127.0.0.1` 时来源 IP 是回环，只有 `Host` 还能暴露真相。

## 诊断端点

| 端点 | 正常响应 | 说明 |
| --- | --- | --- |
| `GET /health` | `{"status":"healthy","timestamp":...,"uptimeSecs":...,"version":...,"mode":"proxy"}` | 只证明进程在跑 |
| `GET /status` | `{"status":"running\|stopped","mode":"proxy","bind":...,"port":...,"gatewayUrl":...,"startedAt":...,"uptimeSecs":...,"requestCount":...,"errorCount":...}` | 计数从进程启动算起 |
| `GET /internal/quota` | `{"quota":{...}}` 或 `{"quota":null}` | 只读本地库的最近一次 `global` 快照，不触发网络 |
| `GET /v1/models`、`/models` | `{"object":"list","data":[{"id":...,"object":"model","created":0,"owned_by":"codemaker"}]}` | 目录为空时 503 `service_unavailable`，提示先登录并完成 `/validate` |
| `POST /v1/messages/count_tokens` | `{"input_tokens":<n>}` | 本地估算，见上一节 |

`/status` 与 `/internal/quota` 都含运行期信息，因此同样受上面两道守卫保护。

## 排障速查

| 客户端行为 | 代理响应 | 通常原因与下一步 |
| --- | --- | --- |
| 连不上 `127.0.0.1:15721` | 无响应 | 端口被回退或代理没起来；读 `~/.codemaker-hub-light/proxy.port` 拿实际端口，再试 `proxy_restart` |
| `GET /health` 通，转发全部失败 | 502 `upstream_unavailable` | 网关地址没填 / 填错 / 该地址不可达 |
| 请求带 `Origin` | 403 `origin_not_allowed` | 用 CLI 或 SDK 直连，浏览器页面不允许调用 |
| 用自定义域名访问 `127.0.0.1` | 403 `host_not_allowed` | `Host` 必须是回环；改回 `127.0.0.1:<port>` |
| 未登录就请求 | 401 `authentication_error` | 先在应用里登录 |
| 一次塞很大的上下文 | 413 `request_too_large` | 超过 32 MiB；拆请求或改用更小的附件 |
| `POST /v1/messages` 体有问题 | 400 `invalid_request_error` | 看 message：非法 JSON / 缺 model / messages 为空 / content 类型错误 |
| 上游 120 s 没回头 | 504 `upstream_timeout`（「上游首字节超时」） | 上游排队或模型太慢；重试或换模型 |
| 上游返回 302 | 502 `upstream_redirect` | 网关地址配成了会跳转的入口，改成直连地址 |
| 流式响应突然截断且用量里有 `timeout` | 头已经发出，只能表现为断流 | 上游相邻 chunk 间隔超过 300 s |
| 中途 Ctrl-C | 用量里 `error_kind = client_aborted` | 正常现象，其余请求不受影响 |
| 请求未知路径 | 404 `not_found` | 响应体里带 `path`，对照路由表 |
| `GET /v1/models` 返回 503 | `service_unavailable` | 模型目录未缓存：先登录并完成 `/validate`（含 `/models` 别名）|
| 用量页 token / 费用为 0 | 200，但 usage 为空 | 上游没回 `usage`，或非流式超 8 MiB / SSE 超 2 MiB 已停止解析 |

## 源码索引

- `src-tauri/src/proxy/mod.rs`
- `src-tauri/src/proxy/server.rs`
- `src-tauri/src/proxy/routes/mod.rs`
- `src-tauri/src/proxy/routes/forward.rs`
- `src-tauri/src/proxy/routes/stream_proxy.rs`
- `src-tauri/src/proxy/routes/diagnostics.rs`
- `src-tauri/src/proxy/routes/errors.rs`
- `src-tauri/src/proxy/routes/context.rs`
- `src-tauri/src/proxy/forward.rs`
- `src-tauri/src/proxy/host_guard.rs`
- `src-tauri/src/proxy/policy.rs`
- `src-tauri/src/proxy/tee.rs`
- `src-tauri/src/proxy/sse_frames.rs`
- `src-tauri/src/proxy/recorder.rs`
- `src-tauri/src/proxy/usage_parser.rs`
- `src-tauri/src/proxy/credentials.rs`
- `src-tauri/src/proxy/translate/mod.rs`（同目录另有 `request.rs`、`response.rs`、`stream.rs`、`error.rs`、`sse.rs`）
- `src-tauri/src/settings.rs`
- `src-tauri/src/bootstrap.rs`
- `src-tauri/src/commands/proxy.rs`
