# 设置项与本地配置文件

> 这篇讲 `settings.json` 的六个配置键（类型、默认值、校验、生效方式）、原子写入与坏文件自愈，以及 `proxy.port` 端口文件；读者是使用者和维护者。

## 文件位置

| 项 | 值 |
|---|---|
| 数据目录 | `~/.codemaker-hub-light`（Windows 为 `%USERPROFILE%\.codemaker-hub-light`） |
| 设置文件 | `<数据目录>/settings.json` |
| 端口文件 | `<数据目录>/proxy.port` |
| 日志目录 | `<数据目录>/logs/`（`hub-light.log` 按日滚动） |

数据目录由 `settings::default_data_dir()` 定位：优先环境变量 `USERPROFILE`，其次 `HOME`，都取不到时返回 `SettingsError::HomeDirUnavailable` 并弹窗退出（窗口还没创建）。目录用 `create_dir_all` 幂等创建。

`settings.json` 只在保存设置时创建。启动读取走 `Settings::load`：文件不存在直接返回 `Settings::default()`，不写盘；`load_or_recover` 也不会为了“补齐默认值”而落盘。因此全新安装的数据目录里没有 `settings.json` 属于正常现象。

## 写入方式：临时文件 + fsync + rename

`Settings::save` 的步骤固定（`settings.rs::atomic_write`）：

1. `ensure_dir` 创建数据目录；
2. `serde_json::to_vec_pretty` 序列化（带缩进，便于用户手改）；
3. 写入同目录临时文件 `settings.json.tmp`；
4. `write_all` 之后 `sync_all`（fsync）确保内容真正落盘；
5. `rename` 覆盖 `settings.json`（Windows 上 rename 可覆盖已存在文件）；
6. 任一步失败：删除临时文件，返回 `SettingsError::Io`，原文件保持不动。

只写 `settings.json.tmp` 再改名，是为了避免进程崩溃/断电留下半截 JSON——那种文件只能靠下一节的“自愈”收拾，而自愈会丢掉用户全部配置。

### 写入口与锁语义

进程内唯一的写入口是 `SettingsStore::update`（IPC 命令 `settings_update`）。顺序固定为：

1. 取写者异步 `Mutex`（并发保存串行化，后到者覆盖前者而不是交叉写）；
2. 读取当前设置克隆一份，`apply_patch` 合并前端提交的局部字段，`normalize`，`validate`；
3. **锁外**落盘（`save`）；
4. 成功后替换内存设置；
5. 把新设置投影到运行期代理策略 `PolicyCell`。

设置读锁不跨 `to_vec_pretty` / `fsync` / `rename`，因此保存设置不会阻塞认证后台循环、深链处理或代理读配置。任一步失败（校验不过或落盘失败）时，内存与运行期策略都保持旧值，代理下一次请求读到的仍是旧策略。

## 配置键

```text name=settings-keys
gateway_url
proxy_bind
proxy_port
record_body
retention_days
credit_to_yuan
```

字段名是 `snake_case`，与前端 `AppSettings` 一致。`SettingsPatch` 的每个字段都是 `Option`：没提交的键保持原值，不会被重置成默认值。

### `gateway_url`

| 项 | 说明 |
|---|---|
| 类型 | `String` |
| 默认值 | `https://api.example.com`（占位地址，需在设置里改成可访问的兼容网关） |
| 校验 | `https://` 允许任意主机；`http://` 只允许 `127.0.0.1` / `localhost` / `::1`（可带端口，兼容 `[::1]:8080` 写法）；其他协议或无协议前缀一律拒绝 |
| 规范化 | 保存与启动时 `trim`，并去掉末尾一个或多个 `/` |
| 生效方式 | 登录、轮询、`/validate`、额度、24h 云端曲线在每次命令中读取当前值，**即时生效**；本地代理的转发目标在启动时固化，**需重启代理** |
| 设置页 | 可编辑 |

校验源码是 `gateway_url_is_acceptable`。明文 `http://` 只对本机回环放行，因为账号凭据以请求头形式发往网关。

### `proxy_bind`

| 项 | 说明 |
|---|---|
| 类型 | `String` |
| 默认值 | `127.0.0.1` |
| 校验 | 去空白后必须等于 `127.0.0.1`、`localhost`、`::1` 之一（`is_loopback_bind`） |
| 生效方式 | 仅在新监听时读取，**需重启代理** |
| 设置页 | 无编辑入口，只读展示；需要改动时手工编辑文件 |

注意两层校验并不一致：设置层接受 `localhost`，而代理启动时还要把绑定地址解析成 `IpAddr`（`ProxyServer::start_locked`），`localhost` 不是 IP 字面量，会在重启代理时报“监听地址不是合法 IP”。也就是说该值写 `localhost` 能通过保存，但代理起不来；实际可用值是 `127.0.0.1` 或 `::1`。

### `proxy_port`

| 项 | 说明 |
|---|---|
| 类型 | `u16` |
| 默认值 | `15721` |
| 校验 | `>= 1024`；上限由 `u16` 决定（65535）。JSON 里写负数或超过 65535 会反序列化失败，启动时按坏文件处理 |
| 生效方式 | 仅在新监听时使用，**需重启代理** |
| 设置页 | 可编辑（前端另有一条 1024~65535 的提示文案） |

端口被占用时的回退顺序（`proxy/server.rs::bind_listener`）：配置值若 `<= 15800`，依次尝试 `配置值..=15800`，最后追加 `0`（交给系统分配临时端口）；配置值 `> 15800` 时只尝试该端口和 `0`。回退会打一条 warn 日志“首选端口不可用，已回退”，并把实际端口写进 `proxy.port`。

### `record_body`

| 项 | 说明 |
|---|---|
| 类型 | `bool` |
| 默认值 | `false`（隐私优先，正文默认不入库） |
| 校验 | 无额外规则（类型不符会反序列化失败） |
| 兼容 | 旧文件里的 `recordBodies` / `recordBody` 通过 `serde(alias)` 读入 |
| 生效方式 | **即时**：保存时写回 `PolicyCell`，下一次请求取到的策略即为新值 |
| 设置页 | 可编辑（开关） |

开启后请求/响应正文按 `policy.max_body_bytes`（默认 64 KiB）截断写入 SQLite，按 UTF-8 字符边界安全截断，被截断时请求行 `body_truncated = 1`。正文可能包含代码等敏感内容。

### `retention_days`

| 项 | 说明 |
|---|---|
| 类型 | `u32` |
| 默认值 | `90` |
| 校验 | `1..=3650` |
| 生效方式 | **下一轮维护**：启动后立即跑一次，之后每 24h 一次；每轮从设置快照读取该值（无需重启）。维护内部还会再 `clamp(1, 3650)` |
| 设置页 | 可编辑 |

只影响本机 SQLite 的请求明细、会话、日聚合与额度快照；`exports/` 下已导出的 CSV 不会被删除。

### `credit_to_yuan`

| 项 | 说明 |
|---|---|
| 类型 | `f64` |
| 默认值 | `0.01` |
| 校验 | 必须是有限数且 `> 0`（`NaN`、`inf`、0、负数都拒绝） |
| 生效方式 | **即时**：每请求策略快照参与 `aigw.cost.total_credit → 人民币元` 换算；费用曲线返回值也读取当前值 |
| 设置页 | 无编辑入口；需要改动时手工编辑文件 |

它只影响“上游给出积分”的记录（`cost_source = upstream`）；没有上游费用时当前代码不会用 `model_prices` 估算。

## 旧键与兼容

旧版 `settings.json` 里已删除的三个键 `quota_refresh_secs`、`validate_interval_secs`、`log_level` 会被 serde 直接忽略：结构体没有加 `deny_unknown_fields`，这几个配置此前也从未被运行时读取，删除不改变行为，因此升级不需要迁移文件。`recordBodies` / `recordBody` 则会映射到 `record_body`。

## 坏文件自愈

只在**启动期**自愈：`bootstrap::init_state` 调用 `Settings::load_or_recover`。运行期的 `settings_update` 校验失败会直接返回错误，不会产生坏文件。

触发条件（满足任一）：

1. `settings.json` 不是合法 JSON（手改漏了逗号、被其他程序截断等）；
2. JSON 合法但 `validate` 失败（例如 `credit_to_yuan: 0`、明文 `http://` 网关且不是回环地址、`proxy_bind: 0.0.0.0`）；
3. 读取文件时报 `NotFound` 之外的 IO 错误（例如权限不足）。

处理步骤：

1. 把原文件 `rename` 成备份：`settings.json.bad-<Unix 秒>`（同一目录内，秒级时间戳，取自 `SystemTime::now()`）；
2. 内存回退到 `Settings::default()`（六个键全部默认值）；
3. 打一条 warn 日志，字段为 `reason`、`backup`、`backed_up`，消息是“设置文件不可用，已回退默认值”。

因此 `logs/` 下当日的 `hub-light.log` 滚动文件里能看到类似：

```text
WARN ... 设置文件不可用，已回退默认值 reason="解析设置文件失败：..." backup="...settings.json.bad-1760000000" backed_up=true
```

若备份的 `rename` 本身失败（例如文件被占用），日志里 `backed_up=false`，原文件原地保留，但本次启动仍使用默认值——应用不会因为坏配置打不开。

两个覆盖该行为的单元测试：`corrupt_settings_recover_to_defaults_with_backup`（坏 JSON 会备份并回默认值）与 `invalid_settings_recover_to_defaults`（合法 JSON 但校验失败同样自愈）。

## `proxy.port` 文件

| 项 | 说明 |
|---|---|
| 谁写 | `ProxyServer::start_locked`：代理启动/重启、绑定成功后调用 `settings::write_proxy_port`；写失败只打 warn（“写入 proxy.port 失败”），不影响代理继续服务 |
| 写什么 | 实际监听端口的十进制文本，无换行（如 `15721`；发生回退时可能是 `15722..15800` 或系统分配的端口） |
| 写入方式 | 与 `settings.json` 相同的原子写：`proxy.port.tmp` + fsync + rename |
| 给谁读 | 当前代码里应用自身不回读；供外部客户端/脚本发现实际端口。路径也通过 `app_info.proxy_port_file` 与 `proxy_status.port_file` 暴露给界面 |
| 与配置的关系 | `settings.proxy_port` 是“首选值”，`proxy.port` 是“实际值”；端口回退时两者不相等，设置页据此提示 |

## 键总表

| 键 | 类型 | 默认值 | 校验 | 生效方式 | 设置页可改 |
|---|---|---|---|---|---|
| `gateway_url` | `String` | `https://api.example.com` | `https://` 任意主机；`http://` 仅回环 | 命令即时 / 代理需重启 | 是 |
| `proxy_bind` | `String` | `127.0.0.1` | 仅 `127.0.0.1` / `localhost` / `::1` | 需重启代理 | 否 |
| `proxy_port` | `u16` | `15721` | `1024..=65535` | 需重启代理 | 是 |
| `record_body` | `bool` | `false` | 无额外规则 | 即时（下一次请求） | 是 |
| `retention_days` | `u32` | `90` | `1..=3650` | 下一轮维护（≤24h） | 是 |
| `credit_to_yuan` | `f64` | `0.01` | 有限且 `> 0` | 即时（下一次请求） | 否 |

## 源码索引

| 路径 | 用到的内容 |
|---|---|
| `src-tauri/src/settings.rs` | `Settings` 字段与 `Default`、`validate`、`normalize`、`apply_patch`、`atomic_write`、`load_or_recover`、`write_proxy_port` |
| `src-tauri/src/settings_store.rs` | 唯一写入口、写者锁、落盘/内存/策略的先后顺序 |
| `src-tauri/src/bootstrap.rs` | 启动期 `load_or_recover`、维护任务周期与 `retention_days` 读取 |
| `src-tauri/src/proxy/policy.rs` | `ProxyPolicy::from_settings`、`PolicyCell` 即时生效路径 |
| `src-tauri/src/proxy/server.rs` | 端口回退、`proxy.port` 写入、`gateway_url` 固化在 `RuntimeInfo` |
| `src-tauri/src/commands/settings.rs` | `settings_get` / `settings_update` 的返回与错误包装 |
| `src-tauri/src/db/maintenance.rs` | `retention_days` 消费方与边界计算 |
| `src-tauri/src/state.rs` | 数据目录、`proxy.port` 路径 |
| `src/lib/types.ts` | `AppSettings` 与 `SettingsPatch` 契约 |
| `src/lib/settingsForm.ts` | 设置页可编辑字段与前端校验 |
| `src/pages/settings/index.tsx`、`src/pages/settings/ProxyPanel.tsx` | 保存后的“是否需重启”提示与端口回退展示 |
