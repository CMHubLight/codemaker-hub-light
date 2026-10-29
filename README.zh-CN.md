# CodeMaker Hub Light

[![CI](https://github.com/CMHubLight/codemaker-hub-light/actions/workflows/ci.yml/badge.svg)](https://github.com/CMHubLight/codemaker-hub-light/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/CMHubLight/codemaker-hub-light)](https://github.com/CMHubLight/codemaker-hub-light/releases/latest)
[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue)](LICENSE)
![Platform: Windows](https://img.shields.io/badge/platform-Windows-0078D4)

[English](README.md) · [文档索引](docs/README.md)

用账号在本机提供**账号登录、本地代理、用量统计**的 Windows 桌面应用。

登录一次，把 Claude Code、Codex 或其他支持自定义 Base URL 的 OpenAI/Anthropic 客户端指向本机代理，请求会自动带上你的账号凭据发往你配置的网关；额度、Token 与费用在同一个窗口里看。

> **这是一个独立实现的开源项目**，与任何厂商的产品无隶属、背书或衍生关系。仓库不内置任何服务地址：聊天与模型请求发往**你自己配置的兼容网关**。

```text
Claude Code ─┐
Codex        ├──→ http://127.0.0.1:15721 ───→ 你配置的兼容网关
OpenAI SDK  ─┘     （本地代理：注入账号凭据、转发、记录用量）
```

- 版本：0.1.0
- 平台：Windows 10/11 x64（macOS / Linux 未验证）
- 前置条件：一个可访问的兼容网关（端点约定见 [网关契约](docs/gateway-contract.md)）
- 数据目录：`%USERPROFILE%\.codemaker-hub-light\`

## 它能做什么

- **账号登录**：系统浏览器完成登录，深链自动回跳；没跳回时可以手动粘贴回调链接。凭据加密保存在本机（AES-256-GCM，主密钥在 Windows 凭据管理器），支持自动续期；token 过期时会先用 `auth_key` 自动续期一次再判失效，设置页也可手动「重新检测 / 续期」。退出登录即清除凭据。
- **本地代理**：默认监听 `127.0.0.1:15721`，被占用时回退 `15722–15800`（再失败交给系统分配）；接受 Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 三类请求，支持流式返回。客户端自带的鉴权头（`authorization`、`x-api-key`、`api-key`、`x-goog-api-key` 等）会被剥离，替换成本机保存的账号凭据。入口只接受回环 `Host`（防 DNS rebinding 页面读取 `/status` 与额度），带 `Origin` 的请求一律 403。
- **用量统计**：本机维度看今日 Token/费用、30 天费用曲线、逐请求明细与 CSV 导出；账号维度看月度额度、本月 Token/消耗、模型分布和最近 24 小时费用曲线。
- **托盘常驻**：关闭窗口只隐藏，代理与统计继续运行；托盘提示显示本月积分；重复启动只聚焦已有窗口。

**不包含**：Agent 编排与接管、聊天会话、MCP/技能/插件市场、Hook、代码地图、规格驱动开发、定时任务与通知、自动更新、桌宠/悬浮球。本应用只做上面四件事。

## 快速开始

### 1. 获取应用

- **从源码构建**（当前唯一分发方式）：见文末「从源码构建」。
- 首次启动会在 `%USERPROFILE%\.codemaker-hub-light\` 下创建数据目录、SQLite 数据库和日志目录；代理启动后写入 `proxy.port`；登录成功后生成加密凭据 `auth.bin`；修改设置后才写入 `settings.json`。

### 2. 配置网关（必须先做）

默认网关地址是占位值 `https://api.example.com`，**不配置就无法登录、也无法转发**。

1. 打开 **设置 → 代理与记录**；
2. 在「网关地址」填入你可访问的兼容网关，保存；
3. 点 **重启代理** 让网关生效。

兼容网关需要实现的端点见 [docs/gateway-contract.md](docs/gateway-contract.md)。

### 3. 登录

1. 点「登录」。
2. 系统浏览器打开登录页，完成登录。
3. 浏览器会尝试拉起应用；应用切到概览页即登录成功。

**浏览器没有自动跳回应用？** 回到登录页，展开底部「浏览器没有自动跳回应用？手动粘贴回调链接」，粘贴地址栏里的 `codemakerhub-light://auth/callback?...` 链接后点「提交」。回调链接必须属于**当前这次**登录会话：过期、已用过或来自上一次登录的链接会被拒绝。

> 回调链接含一次性登录凭据，不要截图、粘贴到聊天或工单里。

登录成功的标志：概览页额度卡出现数字；左下角显示登录账号与有效期；设置 → 账户显示「已登录」、账号、部门与凭据状态。

### 4. 确认代理在运行

打开 **设置 → 本地代理**：

- 状态应为「运行中」；
- 「客户端 Base URL」就是客户端要填的地址，默认 `http://127.0.0.1:15721`；
- 15721 被其他程序占用时会自动回退，**一律以界面显示的端口为准**。

命令行自检：

```powershell
curl.exe http://127.0.0.1:15721/health
curl.exe http://127.0.0.1:15721/v1/models
```

- `/health` 返回 200 表示代理存活；
- `/v1/models` 登录前返回 503（模型目录尚未缓存），登录并完成校验后返回 OpenAI 格式的模型列表；
- 未登录时转发接口返回 401。

## 接入客户端

先到 **设置 → 本地代理** 复制「客户端 Base URL」。下面示例使用默认端口 `15721`；如果你的界面显示其他端口，请同步替换。

### Claude Code

```powershell
$env:ANTHROPIC_BASE_URL="http://127.0.0.1:15721"
$env:ANTHROPIC_AUTH_TOKEN="codemaker-hub-local"
claude
```

- key 只是占位符：代理会剥离客户端自带的 `authorization` / `x-api-key`，换成本机保存的账号凭据。
- 如果客户端读取的是 `ANTHROPIC_API_KEY`，填同一个占位值即可。
- `/v1/messages` 会在本地转换为 OpenAI Chat 再发往网关，支持流式返回与 `tool_use`；`thinking`、`cache_control`、`documents` 等 Anthropic 专有字段会被丢弃。
- `/v1/messages/count_tokens` 返回本地估算值，不请求上游。

### Codex

```toml
# ~/.codex/config.toml（示例，按你的 Codex 版本调整）
model_provider = "codemaker"

[model_providers.codemaker]
name = "CodeMaker Hub Light"
base_url = "http://127.0.0.1:15721/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
```

```powershell
$env:OPENAI_API_KEY="codemaker-hub-local"
codex
```

### OpenAI SDK

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:15721/v1",
    api_key="codemaker-hub-local",  # 占位；代理会剥离并注入账号凭据
)

resp = client.chat.completions.create(
    model="deepseek-flash",  # 以 /v1/models 返回的 id 为准
    messages=[{"role": "user", "content": "hi"}],
)
print(resp.choices[0].message.content)
```

模型名以 `curl.exe http://127.0.0.1:15721/v1/models` 返回的 id 为准；实际路由到哪个模型由网关决定。

### 状态码速查

| 现象 | 含义 |
|---|---|
| `/health` 200 | 代理存活 |
| `/v1/models` 503 | 还没登录，或模型目录尚未缓存 |
| `/v1/models` 200 | 模型目录可用 |
| 转发接口 401 | 未登录或凭据已失效，重新登录 |
| 403 `origin_not_allowed` | 请求带了 `Origin` 头（典型是浏览器页面直接 fetch）；代理只接受本机 CLI/SDK |
| 404 | 路径不在支持列表内 |

支持路径：`/v1/messages`（含 `/count_tokens`）、`/v1/chat/completions`、`/v1/responses`、`/v1/models`；这些路径同时接受去掉 `/v1` 的写法。另有诊断用的 `/health`、`/status`、`/internal/quota`。

> 只有经过本地代理的请求才会进入「今日」和「请求」页；直接访问网关或用其他代理的请求不会记录。

## 界面导览

### 概览

- **月度额度卡**：账号云端的月度额度、已用/剩余、重置日；点「刷新额度」立即拉取。失败时保留上次成功数据并标注「旧数据」。
- **今日 Token / 今日费用**：只统计经过本机代理的请求，费用单位为元。
- **本月 Token / 本月消耗**：来自账号云端的月度统计，消耗单位是**积分**（不是元）；这是账号级数据，除本机代理外还可能包含其他客户端与设备的用量。
- **费用曲线**：默认「24 小时」，来自账号云端积分快照；切到「30 天」读取本机代理的每日费用（元）。
- **模型分布**：账号云端本月的各模型 Token 与积分消耗。
- **最近请求**：本机代理最近记录的请求。

### 请求

按时间范围、协议、状态、模型关键字筛选，分页查看逐请求的输入/输出 Token、费用、耗时与状态。点「导出 CSV」按当前时间范围导出到 `%USERPROFILE%\.codemaker-hub-light\exports\usage_<起>_<止>.csv`（UTF-8 BOM，Excel 可直接打开）；导出只看时间范围，不应用协议/状态/模型筛选，选「全部」时导出最近 30 天，单次上限 20 万行。费用列固定 4 位小数；模型名等文本若以 `=` `+` `-` `@` 开头会被前置单引号中和，避免 Excel 当公式执行。

当前版本没有请求详情弹层；「客户端」列暂未关联会话记录，固定显示 `unknown`。

### 设置

- **本地代理**：运行状态、Base URL、健康检查地址、运行时长、请求/错误计数、端口回退提示；修改端口或网关后需先保存，再点「重启代理」。
- **代理与记录**：代理端口、网关地址、用量保留天数、是否记录请求正文（默认关闭）。记录正文开关与积分换算保存后立即生效；端口与网关需要重启代理。
- **账户**：账号、部门、登录网关、签发/有效期、最近校验、凭据状态；「退出登录」清除本地凭据，代理立即返回 401。
- **接入说明**：可直接复制的 Claude Code / Codex / OpenAI SDK 配置片段。
- **应用信息**：版本、标识、数据目录、数据库、Schema 版本、日志目录、端口文件；「打开日志目录」直达日志。

### 托盘与窗口

- 关闭窗口（X / Alt+F4）只是隐藏到托盘，代理和统计继续运行。
- 托盘 tooltip 显示「本月积分 x% · 已用 / 总额」，数据来自本地额度快照，不额外发起网络请求。
- 托盘右键只有「打开主界面」「退出」；左键单击也会打开主界面。
- 要完全退出必须走托盘「退出」；退出后本地代理停止，客户端请求会失败。
- 重复双击 exe 只会聚焦已打开的窗口，不会启动第二个实例。

## 数据与隐私

下表路径均位于 `%USERPROFILE%\.codemaker-hub-light\`：

| 路径 | 内容 |
|---|---|
| `auth.bin` | AES-256-GCM 加密的登录凭据，主密钥存放在 Windows 凭据管理器 |
| `codemaker-hub-light.db` | SQLite（WAL）：请求、费用、日聚合、额度快照、模型目录缓存 |
| `settings.json` | 修改过的设置；未改过设置时不会生成 |
| `proxy.port` | 代理实际监听端口（纯文本，供客户端/脚本读取） |
| `logs\hub-light.log.<日期>` | 按日滚动日志 |
| `logs\panic.log` | 崩溃记录：panic 的 `文件:行:列`、消息与回溯（仅崩溃时生成） |
| `logs\startup-error.log` | 启动期致命错误现场 |
| `exports\usage_<起>_<止>.csv` | 请求页导出的用量 |

- **凭据**：token 只保存在 Rust 内存与加密文件里；前端、日志、数据库都不出现明文 token；加密不可用时会报错并拒绝加载凭据，不会降级为明文。
- **记录**：默认不记录请求/响应正文；开启后按 64 KB 截断写入本地数据库，可能包含代码等敏感内容。请求头只记录白名单字段（UA、协议版本、会话 id 等），不含凭据。
- **网络**：代理只监听回环地址；带 `Origin` 的请求一律 403；单请求正文上限 32MB。
- **保留与清理**：本地请求明细按「用量保留天数」在启动后清理一次、之后每 24 小时清理一次（保留期当天的数据不删）；额度快照按类型保留最近若干条。已导出的 CSV 不受影响。
- **卸载**：安装器只移除安装目录；如需彻底清理，请手动删除数据目录。

## 代码签名政策（Code signing policy）

发布安装包计划通过 SignPath Foundation 签名：**Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org)**。

| 版本 | 安装包 |
|---|---|
| `v0.1.0` | **未签名**——请用 Release 说明里的 SHA-256 校验，或从 tag 源码自行构建 |
| 后续版本 | 经维护者人工批准后由 SignPath Foundation 签名 |

证书签发给 SignPath Foundation，因此 Windows 上显示的签名者是 `SignPath Foundation`。可用 `Get-AuthenticodeSignature` 或 `signtool verify /pa` 验签，也可比对每次发布附带的 SHA-256。

角色（当前单人维护）、隐私声明、安装/卸载的系统改动与完整签名流程见 [docs/code-signing-policy.md](docs/code-signing-policy.md)。

## 常见问题

完整排查见 [docs/troubleshooting.md](docs/troubleshooting.md)，这里只列最高频的几个：

**客户端连不上，或端口不是 15721**
看「设置 → 本地代理」的实际端口，把客户端 Base URL 改成实际端口；端口回退时界面会提示。

**登录后什么都打不开 / 额度一直失败**
先确认「设置 → 代理与记录」里的网关地址已填写且可访问——默认值只是占位地址。

**浏览器回调后没有跳回应用**
用登录页底部的「手动粘贴回调链接」提交；链接含一次性凭据，不要外传。

**点回调链接提示"没有打开此链接的应用"**
深链 scheme 在应用启动时写入注册表，安装后必须先运行一次应用：

```powershell
reg query HKCU\Software\Classes\codemakerhub-light /s
```

**请求页是空的**
只有经过本地代理的请求才会入库；确认客户端 Base URL 指向本机代理、代理在运行。

**本月数字和本机统计对不上**
月度数据来自账号云端、可能包含其他设备；「今日」与「请求」页只统计本机代理。两者口径不同，不是同一份数据。

**应用无声退出 / 双击没反应 / 整窗白屏**
依次看 `logs\panic.log`、`logs\startup-error.log`、`logs\hub-light.log`；反馈前先脱敏。

## 从源码构建

需要：Windows 10/11 x64、Node.js ≥ 20.19、pnpm、Rust stable（MSVC toolchain）、Visual Studio Build Tools + Windows SDK、WebView2 Runtime。

```powershell
pnpm install
pnpm tauri dev                 # 开发模式：前端 5173 + Rust 后端
pnpm tauri build               # 产出 NSIS 安装包
pnpm tauri build --no-bundle   # 只产出可执行文件
```

产物路径：

```text
src-tauri/target/release/codemaker-hub-light.exe
src-tauri/target/release/bundle/nsis/CodeMaker Hub Light_0.1.0_x64-setup.exe
```

> **不要单独用 `cargo build --release` 生成可运行版本**：Tauri v2 需要 `custom-protocol` feature 才会内嵌前端，否则窗口会显示"无法访问此页面"。详见 [docs/development.md](docs/development.md)。
>
> **不要给测试加 `--release`**：release 是 `panic = "abort"`，测试线程无法 unwind，cargo 会直接拒绝。

其他门禁命令：

```powershell
pnpm verify                                    # 类型检查 + lint + 测试 + 构建 + 文档门禁
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
node scripts/check-public-repo.mjs             # 脱敏 / 链接 / 文档与代码一致性
```

## 当前状态与已知限制

当前版本 0.1.0，仅支持 Windows（NSIS 安装包），无自动更新。

- 原生 Anthropic 上游不受支持，`/v1/messages` 必须走本地翻译；`thinking`、`cache_control`、`documents` 等字段会被丢弃，模型由网关实际路由。
- 上游未返回费用字段时费用记为 0（`cost_source=none`），暂无本地价格表兜底。
- 自动续期成功后短期不再续期；若网关连续多次返回的有效期没有前移，会提示重新登录而不是继续空转。
- 「用量保留天数」只清理数据库，不清理已导出的 CSV。
- 请求页分页基于游标（keyset）：并发写入下不会重复/漏行；「共 N 条」只在第一页与筛选变化时统计。
- 请求页没有详情弹层；「客户端」列固定显示 `unknown`。
- 「今天 / 本月」按本机本地日统计，跨本地午夜的请求计入开始那天。
- 深链 scheme 在应用启动时写入 `HKCU\Software\Classes`，安装后必须先运行一次应用。
- 仅验证 Windows；macOS / Linux 未验证。

## 文档

| 文档 | 内容 |
|---|---|
| [文档索引](docs/README.md) | 全部文档与阅读顺序 |
| [总体架构](docs/architecture.md) | 进程/模块、启动时序、数据流、不变量 |
| [认证与凭据](docs/authentication.md) | 登录时序、深链、加密存储、续期 |
| [本地代理](docs/proxy.md) | 路由、头处理、协议翻译、上限、状态码 |
| [网关契约](docs/gateway-contract.md) | 兼容网关需要实现的端点与字段 |
| [用量统计](docs/usage.md) | token/费用口径、聚合、保留期、CSV |
| [设置项](docs/settings.md) | `settings.json` 全量键与校验规则 |
| [数据模型](docs/data-model.md) | 表、索引、迁移 |
| [IPC 接口](docs/ipc-api.md) | 命令与事件 |
| [开发与构建](docs/development.md) | 脚本、门禁、CI、打包坑 |
| [排障手册](docs/troubleshooting.md) | 症状 → 原因 → 动作 |
| [代码签名政策](docs/code-signing-policy.md) | 签名提供方、验签方法、角色、隐私与系统改动 |

## 贡献

欢迎 issue 与 PR。提 PR 前请先跑 `pnpm verify` 与 Rust 三件套（见 [CONTRIBUTING.md](CONTRIBUTING.md)）；`docs:check` 门禁会拒绝新增的内网地址、账号、密钥，以及和代码不一致的文档。参与本项目即表示同意遵守[行为准则](CODE_OF_CONDUCT.md)。

## 许可与项目关系

本项目以 GPL-3.0-only 发布，见 [LICENSE](LICENSE)。第三方依赖各自保留其许可证（见 `pnpm-lock.yaml`、`src-tauri/Cargo.lock`）。

本仓库是独立实现：应用代码不包含任何第三方产品的业务代码，也不随仓库分发其前端资源、文档或安装产物。需要指代服务端时，统一称"上游网关"或"兼容网关"，由使用者自行配置。
