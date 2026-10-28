# 贡献指南

感谢你愿意改进这个项目。先花两分钟读完下面几节，能省掉一轮返工。

## 1. 先确认改动在范围内

本项目只做三件事：**账号登录**、**本地代理**（协议转发 + 鉴权注入）、**用量统计**。

不在范围内：Agent 编排、聊天会话、MCP/技能/插件市场、Hook、代码地图、定时任务、自动更新。提这类 PR 前请先开 issue 讨论。

## 2. 环境

Windows 10/11 x64、Node.js ≥ 20.19、pnpm、Rust stable（MSVC）、Visual Studio Build Tools + Windows SDK、WebView2 Runtime。

```powershell
pnpm install
pnpm tauri dev
```

只调前端可以 `pnpm dev`（浏览器预览，全部走 mock，不能登录/代理）。

## 3. 提交前必须过的门禁

```powershell
pnpm verify                                    # typecheck + lint + test + build + docs:check
cargo fmt    --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test   --manifest-path src-tauri/Cargo.toml
```

几个容易踩的点：

- **不要给测试加 `--release`**：release 是 `panic = "abort"`，cargo 会直接拒绝 `cargo test --release`。
- **不要用 `cargo build --release` 出可运行包**：缺少 `custom-protocol` feature 会导致窗口白屏，用 `pnpm tauri build`。
- 性能采样测试默认 `#[ignore]`，需要手动 `--ignored --nocapture`。

细节见 [docs/development.md](docs/development.md)。

## 4. 代码约定

| 区域 | 约定 |
|---|---|
| Rust | `rustfmt` 默认风格；`clippy` warnings 视为错误；错误用 `thiserror`，命令层返回 `Result<_, String>` 面向用户 |
| 异步 | 阻塞 IO 不进 async 任务：数据库访问走 `db::blocking::run`（`spawn_blocking`） |
| 前端 | TypeScript strict；ESLint 9 flat config；组件测试用 jsdom + Testing Library |
| 命名 | 代码标识符英文；用户可见文案中文；与既有风格保持一致 |
| 注释 | 只解释"为什么"和不显然的约束；不要在注释里写外部部署地址、账号或本机路径 |

## 5. 改动必须同步文档

改到下列任一项时，`docs/` 对应文件必须一起改，否则 `docs:check` 会失败：

| 改动 | 必须同步 |
|---|---|
| Tauri 命令 / 事件 | [docs/ipc-api.md](docs/ipc-api.md) 的清单块与条目 |
| HTTP 路由 | [docs/proxy.md](docs/proxy.md) 的 `proxy-routes` 清单块 |
| `settings.json` 键 | [docs/settings.md](docs/settings.md) 的 `settings-keys` 清单块 |
| 数据库表 / 迁移 | [docs/data-model.md](docs/data-model.md) 的 `db-tables` 清单块 |
| 用户可见行为 | 相关文档 + [CHANGELOG.md](CHANGELOG.md) |

文档规范见 [docs/README.md](docs/README.md)。

## 6. 提交与 PR

- 提交信息用 Conventional Commits：`feat(proxy): …`、`fix(auth): …`、`docs: …`、`chore: …`。
- 标题一行说清"改了什么"，正文写"为什么"；一个 PR 只做一件事。
- PR 描述里勾选门禁命令，说明手工验证步骤；改用户可见行为时附前后对比。
- 不要在仓库里提交任何真实地址、账号、密钥、日志或本机绝对路径。

## 7. 安全相关

发现安全问题时**不要开公开 issue**，按 [SECURITY.md](SECURITY.md) 的方式私下报告。

## 8. 从哪里开始

- 想熟悉代码：先读 [docs/architecture.md](docs/architecture.md)，再从 `src-tauri/src/proxy/routes/mod.rs` 与 `src/lib/ipc.ts` 各挑一个入口读下去。
- 想找活干：issue 里标了 `good first issue` 的通常是文档、测试或小型前端改动。
