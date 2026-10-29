# 开发与构建

> 这篇讲怎么把项目跑起来、提交前必须过哪些门禁、打包时有哪些坑；给贡献者与维护者看。

## 1. 环境要求

| 组件 | 版本 / 说明 |
|---|---|
| 系统 | Windows 10/11 x64（当前唯一验证平台；macOS / Linux 未验证） |
| Node.js | ≥ 20.19（CI 用 24） |
| pnpm | 11（`packageManager` 未固定，CI 用 `pnpm/action-setup` 的 11） |
| Rust | stable，MSVC 工具链 |
| C++ 工具链 | Visual Studio Build Tools + Windows SDK（Tauri 链接需要） |
| WebView2 Runtime | Windows 10/11 通常随 Edge 预装 |

只有 VS2015（v140）工具链时也能链接：`src-tauri/src/compat/msvc_crt.rs` 提供了仅 Windows/MSVC 生效的旧 CRT 垫片。

## 2. 常用命令

| 命令 | 作用 |
|---|---|
| `pnpm install` | 安装前端依赖（`pnpm-lock.yaml` 已提交） |
| `pnpm dev` | 只跑前端（<http://localhost:5173>），无后端，全部走 mock；不能登录、不能代理 |
| `pnpm tauri dev` | 开发模式：前端 5173 + Rust 后端 + 真实窗口 |
| `pnpm typecheck` | `tsc --noEmit`（含 vite/vitest 配置） |
| `pnpm lint` | ESLint 9 flat config |
| `pnpm test` | Vitest（jsdom + Testing Library） |
| `pnpm docs:check` | 公开仓库门禁：脱敏扫描、链接检查、文档与代码对账 |
| `pnpm verify` | typecheck → lint → test → build → docs:check |
| `pnpm tauri build` | 产出 NSIS 安装包（内含内嵌前端的 exe） |
| `pnpm tauri build --no-bundle` | 只产出可执行文件 |

Rust 侧：

```powershell
cargo check  --manifest-path src-tauri/Cargo.toml
cargo test   --manifest-path src-tauri/Cargo.toml
cargo fmt    --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

## 3. 门禁：提交前必须全绿

| 门禁 | 命令 | CI 位置 |
|---|---|---|
| 前端类型 / lint / 测试 / 构建 | `pnpm typecheck && pnpm lint && pnpm test && pnpm build` | `frontend` job（ubuntu-latest） |
| Rust 格式 / lint / 测试 | `cargo fmt --check`、`cargo clippy -D warnings`、`cargo test` | `rust` job（windows-latest） |
| 脱敏 / 链接 / 文档一致性 | `node scripts/check-public-repo.mjs` | `docs` job（ubuntu-latest） |

`docs` job 检查四件事：

1. **敏感信息**：内网域名、公司/部门名、个人账号与姓名、本机用户目录的绝对路径、来源说明用语、内部错误码。
2. **禁止入库的路径**：不随仓库分发的外部材料与旧版产品文档。
3. **链接**：所有 Markdown 相对链接必须指向**已纳入版本库**的文件。
4. **文档与代码一致**：`docs/ipc-api.md`、`docs/settings.md`、`docs/data-model.md`、`docs/proxy.md` 里的清单块必须与代码逐项相等。

## 4. 测试布局

| 层 | 位置 | 说明 |
|---|---|---|
| Rust 单元测试 | 各模块 `#[cfg(test)]` | 解析、校验、状态机、SQL 行为 |
| Rust 集成测试 | `src-tauri/src/proxy/integration_tests.rs`、`src-tauri/src/auth/service/test_support.rs` | 起真实 Axum 服务 + 假上游，覆盖路由/SSE/落库 |
| Rust 性能采样 | `src-tauri/tests/perf_usage.rs`、`src-tauri/src/proxy/perf_tests.rs` | 默认 `#[ignore]`，手动 `--ignored --nocapture` 才跑 |
| 前端组件测试 | `src/**/*.test.tsx` | jsdom + Testing Library |
| 前端纯函数测试 | `src/lib/*.test.ts` | 格式化、IPC 封装、样式约定 |

> **不要给测试加 `--release`。** release 配置是 `panic = "abort"`，Rust 测试线程依赖 unwind，`cargo test --release` 会被 cargo 直接拒绝。release 产物只能"构建后启动冒烟"。

## 5. 构建与打包的三个坑

### 5.1 不要单独 `cargo build --release`

Tauri v2 的 release 可执行文件需要 `custom-protocol` feature 才会内嵌 `dist/`；直接用 cargo 构建不会启用它，程序会回退到 `build.devUrl`（<http://localhost:5173>），表现为 WebView2 显示"无法访问此页面"。用 `pnpm tauri build`（含 `--no-bundle`）——它会先跑 `pnpm build` 并启用该 feature。

产物：

```text
src-tauri/target/release/codemaker-hub-light.exe
src-tauri/target/release/bundle/nsis/CodeMaker Hub Light_0.2.0_x64-setup.exe
```

### 5.2 `panic = "abort"` 的连带影响

release 关闭 unwind 后，panic 不会跑 `Drop`，日志写入器可能来不及 flush。因此崩溃现场由 `panic_log` 直接 `append + sync_all` 写到 `logs/panic.log`，而不是走 `tracing` 的非阻塞通道。

### 5.3 `LNK4217: locally defined symbol memmove … groupConcatInverse`

这是 bundled SQLite（`rusqlite` 的 `bundled` feature）与旧 CRT 垫片相遇时的**警告**，不影响链接结果，可忽略。它只在 release 链接阶段出现，`cargo check` / `cargo test` 看不到。

## 6. 文档规范

新增或修改行为时，必须同步 `docs/` 下对应文档；文档不是"顺便写写"，有门禁：

1. **清单块**：需要与代码对账的列表必须是 ` ```text name=xxx ` 代码块，一行一项，不带序号与注释。当前约定：

   | 文档 | 清单块 | 对照的代码 |
   |---|---|---|
   | `docs/ipc-api.md` | `ipc-commands` | `src-tauri/src/lib.rs` 的 `generate_handler!` |
   | `docs/ipc-api.md` | `ipc-events` | `src/lib/events.ts` 的 `AppEventName` |
   | `docs/settings.md` | `settings-keys` | `settings.rs` 的 `Settings` 字段 |
   | `docs/data-model.md` | `db-tables` | `migrations/*.sql` 的建表语句 |
   | `docs/proxy.md` | `proxy-routes` | `proxy/routes/mod.rs` 的 `.route()` |

2. **每篇文末有源码索引**，列出该文档依赖的源码文件。
3. **不写来源不明的结论**：以代码与测试为准；无法从代码确认的写"（待验证）"。
4. **不写具体部署地址**：需要指代服务端时统一写"上游网关"或"兼容网关"。

## 7. 提交与 PR

- 提交信息用 Conventional Commits：`feat(proxy): …`、`fix(auth): …`、`docs: …`、`chore: …`；一行标题 + 可选正文，正文写清"为什么"。
- 一个 PR 只做一件事；改变用户可见行为（路由、设置键、状态码、表结构）时在描述里点明，并附文档改动。
- PR 模板里勾选四项门禁命令；CI 会再跑一遍。

## 8. 手工验收清单

自动化门禁覆盖不到"真实窗口 + 真实网关"，发版前手工走一遍：

1. **首次启动**：数据目录下生成数据库、`logs/`、`proxy.port`；改过设置后才出现 `settings.json`；登录后才出现 `auth.bin`。
2. **配置网关**：设置 → 代理与记录 → 填入可访问的兼容网关 → 保存 → 重启代理。
3. **代理存活**：`curl.exe http://127.0.0.1:15721/health` 返回 200；端口被占时界面显示回退后的端口。
4. **登录**：登录 → 浏览器完成 → 深链自动回跳（或手动粘贴回调链接）→ 概览页额度卡出现数字；若网关支持 `POST /api/v2/tokens`，再用「账号 + AuthKey」入口验证一次错误/正确凭据。
5. **模型目录**：登录前 `/v1/models` 返回 503，`/validate` 成功后返回列表。
6. **转发与记录**：用客户端发一次请求 → 请求页出现该条记录（token 与费用非空，首字节列有值）→ 导出 CSV 能打开。
7. **注销**：点退出登录先出现二次确认；取消后仍登录，确认后转发接口返回 401、`auth.bin` 消失。
8. **账户操作**：设置 → 账户点「重新检测 / 续期」成功，`last_validated_at` 刷新。
9. **托盘**：关闭窗口只隐藏；托盘 tooltip 显示本月积分；托盘"退出"后代理停止。
10. **重启保持**：退出再启动，仍是已登录状态（凭据恢复 + `/validate`）。

## 9. 发布与签名

发布产物计划通过 [SignPath Foundation](https://signpath.org) 的免费开源签名做 Authenticode 签名；完整政策（角色、隐私声明、验签方法）见 [代码签名政策](code-signing-policy.md)。在 SignPath 配置/审批完成前，按 §9.4 发布**未签名**包并在 Release 说明中明确标注。

### 9.1 一次性配置

| 位置 | 名称 | 说明 |
|---|---|---|
| SignPath | Organization | 注册并启用 MFA |
| SignPath | GitHub App | 安装 `SignPath` GitHub App，授权本仓库读权限 |
| SignPath | Project | slug 建议 `codemaker-hub-light`，Trusted Build System 选 GitHub.com |
| SignPath | Signing policy | slug 建议 `release-signing`，开启人工批准 |
| SignPath | Artifact configuration | slug 建议 `nsis-installer`，导入 `.github/signpath/artifact-configuration.xml` |
| GitHub → Secrets | `SIGNPATH_API_TOKEN` | SignPath 用户 API Token（submitter 权限） |
| GitHub → Secrets | `SIGNPATH_ORGANIZATION_ID` | SignPath 组织 ID |
| GitHub → Variables | `SIGNPATH_PROJECT_SLUG`、`SIGNPATH_SIGNING_POLICY_SLUG`、`SIGNPATH_ARTIFACT_CONFIGURATION_SLUG` | 与上面三个 slug 对应 |

工作流启动时会先检查这 5 个变量/密钥是否齐全，缺一个就直接失败并提示。

### 9.2 每次发布

1. 打 annotated tag（中性维护者身份）；
2. Actions → **Release signing** → Run workflow，输入 tag；
3. 工作流在 `windows-latest` 构建安装包并提交 SignPath，等待维护者在 SignPath 控制台**人工批准**；
4. 批准后工作流下载已签名产物、`Get-AuthenticodeSignature` 校验、上传/覆盖 Release 附件；
5. 更新 Release 说明里的文件名与 SHA-256。

约束：签名前的构建必须全部跑在 GitHub 托管 runner 上（SignPath 的 OSS 要求）；每个版本都要人工批准一次。

### 9.3 本地无法签名

签名私钥在 SignPath 的 HSM 中，本地只能验签：

```powershell
Get-AuthenticodeSignature .\CodeMaker-Hub-Light_0.2.0_x64-setup.exe | Format-List Status,SignerCertificate
```

不要把任何签名私钥、API Token 或 `.pfx` 提交到仓库。`v0.2.0` 安装包在审批通过前仍是未签名状态，以 Release 说明里的 SHA-256 与 tag 源码为准。

### 9.4 未签名版本的构建、烟雾测试与发布

SignPath 未配置时，发布未签名包也必须走完以下流程（`v0.2.0` 起执行）：

1. **干净构建**：确认 `git status` 干净、从合并后的 `main` 构建；先关闭正在运行的旧实例（否则 Windows 会锁住 exe）。
   ```powershell
   pnpm install --frozen-lockfile
   pnpm tauri build
   ```
2. **烟雾测试**：按 §8 清单手工走真实窗口 + 真实网关；任何失败先修复并重跑 `cargo fmt/clippy/test` 与 `pnpm verify`，再重建。
3. **源码包**：用 `git archive` 从 tag 生成，确保内容与 tag 完全一致（不要用工作区压缩）：
   ```powershell
   git archive --format=zip --prefix=codemaker-hub-light-<version>/ `
     -o codemaker-hub-light-<version>-source.zip v<version>
   ```
   校验：zip 条目数 = `git ls-tree -r --name-only v<version>` 的文件数 + 目录条目；不得包含 `target/`、`node_modules/`、`research/`、本机数据。
4. **校验和**：安装包与源码包都要算 SHA-256，写进 Release 说明：
   ```powershell
   Get-FileHash .\CodeMaker-Hub-Light_<version>_x64-setup.exe -Algorithm SHA256
   Get-FileHash .\codemaker-hub-light-<version>-source.zip -Algorithm SHA256
   ```
5. **发布**：先创建 annotated tag 并推送，再用 `--verify-tag` 创建 Release：
   ```powershell
   gh release create v<version> --verify-tag --title "CodeMaker Hub Light <version>" `
     --notes-file .\release-notes.md `
     .\CodeMaker-Hub-Light_<version>_x64-setup.exe .\codemaker-hub-light-<version>-source.zip
   ```
6. **回验**：从 Release 下载两个附件重算 SHA-256，必须与说明一致；说明中写明「未签名 + 校验方式 + 从 tag 构建」。

## 10. 源码索引

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`
- `src-tauri/src/compat/msvc_crt.rs`
- `src-tauri/src/panic_log.rs`
- `src-tauri/tests/perf_usage.rs`
- `.github/workflows/ci.yml`
- `.github/workflows/release-sign.yml`
- `.github/signpath/artifact-configuration.xml`
- `scripts/check-public-repo.mjs`
