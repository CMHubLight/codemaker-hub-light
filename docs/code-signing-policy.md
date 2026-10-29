# 代码签名政策（Code signing policy）

> 这篇说明本项目的发布产物由谁签名、怎么签、怎么验，以及签名所依赖的隐私与系统改动声明；给下载安装包的用户、上游审核方（SignPath Foundation）和要复现发布的维护者看。

## 1. 当前状态

| 版本 | 安装包状态 | 说明 |
|---|---|---|
| `v0.1.0`（2026-10-27） | **未签名** | Release 页已附 SHA-256；源码 tag 可复现构建 |
| `v0.2.0`（2026-10-29） | **未签名** | Release 页已附 SHA-256；源码 tag 可复现构建 |
| 后续版本 | 计划由 SignPath Foundation 签名 | 申请与接入材料已就绪，等待审批 |

在签名生效前，请用以下任意方式核对 `v0.2.0` 安装包（历史 `v0.1.0` 同理）：

1. 对照 [Release 说明](https://github.com/CMHubLight/codemaker-hub-light/releases/tag/v0.2.0) 里的 SHA-256；
2. 从 tag `v0.2.0` 的源码按 [开发与构建](development.md) 自行构建并比对；
3. 打开文件属性 → 数字签名：当前应为**无签名**，不应出现任何签名者。

## 2. 签名由谁提供

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org).

- 证书签发给 **SignPath Foundation**，因此 Windows 上显示的签名者是 `SignPath Foundation`，不是本项目名称或维护者姓名。
- 私钥保存在 SignPath 的 HSM 中，本项目不持有任何签名私钥。
- 每个版本都需要维护者人工批准后才会签名。

## 3. 签名范围

| 产物 | 是否签名 | 说明 |
|---|---|---|
| NSIS 安装包 `.exe` | 是 | Release 附件中的 `CodeMaker-Hub-Light_<version>_x64-setup.exe` |
| 安装包内的应用主程序 | 否（当前） | NSIS 负载经过压缩，签名服务无法深入签名；安装包签名覆盖其完整性 |
| 源码归档 `.zip` | 否 | 源码由 GitHub 生成，另有平台自动提供的 Source code 链接 |

后续版本会在构建流程中先签应用主程序、再打包并签安装包（deep signing），届时更新本节。

## 4. 团队角色

对应 SignPath 要求的三类角色（本项目当前为单人维护）：

| 角色 | 职责 | 成员 |
|---|---|---|
| Authors / Committers | 可直接修改仓库代码 | [@CMHubLight](https://github.com/CMHubLight) |
| Reviewers | 外部 PR 必须经评审后才能合并 | [@CMHubLight](https://github.com/CMHubLight) |
| Approvers | 批准每一次签名请求 | [@CMHubLight](https://github.com/CMHubLight) |

GitHub 与 SignPath 两端均需启用双因素认证（MFA）；申请前请按第 10 节清单确认已开启。新增维护者或角色变更时，先更新本节再继续签名。

## 5. 发布与签名流程

1. 在 `main` 上打 tag（annotated tag，中性维护者身份）；
2. 触发 **Release signing** 工作流（`.github/workflows/release-sign.yml`），输入 tag；
3. 工作流在 GitHub 托管的 `windows-latest` 上构建安装包，并把未签名产物上传为 workflow artifact；
4. 工作流把 artifact 提交给 SignPath，等待维护者在 SignPath 控制台**人工批准**；
5. 签名完成后下载已签名产物，用 `signtool verify /pa` 校验，再上传/替换 GitHub Release 附件；
6. 在 Release 说明里更新文件名与 SHA-256。

约束：所有签名前的构建步骤都必须跑在 GitHub 托管 runner 上（SignPath 的 OSS 要求），不要改成自托管或本地签名后补传。

所需的 Secrets / Variables 见 [开发与构建](development.md#9-发布与签名)。

## 6. 产物元数据限制

签名请求会随附 `version` 参数（来自 `package.json`），产物配置据此强制校验：

| 属性 | 要求值 | 当前产物 |
|---|---|---|
| `ProductName` | `CodeMaker Hub Light` | ✅ |
| `ProductVersion` | `<version>`（如 `0.2.0`） | ✅ |

产物配置模板见 [`.github/signpath/artifact-configuration.xml`](../.github/signpath/artifact-configuration.xml)，在 SignPath 项目里作为 Artifact Configuration 导入（slug 建议 `nsis-installer`）。

```xml
<artifact-configuration xmlns="http://signpath.io/artifact-configuration/v1">
  <parameters>
    <parameter name="version" required="true" />
  </parameters>
  <zip-file>
    <pe-file path="CodeMaker-Hub-Light_x64-setup.exe"
             product-name="CodeMaker Hub Light"
             product-version="${version}">
      <authenticode-sign />
    </pe-file>
  </zip-file>
</artifact-configuration>
```

> 上传到 SignPath 的 workflow artifact 是 ZIP（`actions/upload-artifact` 的默认行为），所以根元素为 `<zip-file>`；`path` 是 ZIP 内的文件名，因此工作流会先把安装包重命名成不带空格与版本号的固定名字。

## 7. 隐私政策（Privacy policy）

本项目**不收集遥测、不回传任何数据给维护者或第三方**。

| 数据 | 去向 |
|---|---|
| 账号凭据（token） | 只在本机内存与 `auth.bin` 密文中；只发给**你配置的网关** |
| 登录、校验、续期、额度请求 | 只发给你在设置里填写的网关 |
| 代理转发的对话请求 | 只发给你在设置里填写的网关 |
| 请求日志与用量统计 | 只写本机 SQLite；导出 CSV 也只在你指定的本地目录 |
| 日志文件 | 只写本机数据目录 |

This program will not transfer any information to other networked systems unless specifically requested by the user or the person installing or operating it.

上面的"specifically requested"指的就是使用者自己在「设置 → 代理与记录」中填写的网关地址；应用不内置任何服务端点，默认值是占位地址 `https://api.example.com`。

数据目录与各文件的含义见 [README.zh-CN.md](../README.zh-CN.md#数据与隐私)，凭据加密细节见 [认证与凭据](authentication.md)。

## 8. 安装、卸载与系统改动

| 项 | 说明 |
|---|---|
| 安装 | NSIS 安装包，按当前用户安装（不需要管理员权限的机器级服务） |
| 新增文件 | 安装目录；数据目录 `%USERPROFILE%\.codemaker-hub-light\`（数据库、日志、设置、凭据密文） |
| 注册表 | 仅 `HKCU\Software\Classes\codemakerhub-light`（深链回调 scheme），启动时写入 |
| 服务/驱动/计划任务 | 无 |
| 随系统启动 | 无（应用不会把自己加入开机启动） |
| 卸载 | 设置 → 应用 → 已安装的应用 → 卸载；安装目录被移除 |
| 数据残留 | 卸载不删数据目录；如需彻底清理需手动删除 `%USERPROFILE%\.codemaker-hub-light\` |

以上行为都在应用启动时发生，没有隐藏的系统配置修改。深链 scheme 的排查命令见 [排障手册](troubleshooting.md)。

## 9. 如何验证签名

```powershell
# 1) 查看签名状态与签名者
Get-AuthenticodeSignature .\CodeMaker-Hub-Light_0.2.0_x64-setup.exe |
  Format-List Status, StatusMessage, SignerCertificate, TimeStamperCertificate

# 2) 用 signtool 严格校验（需要 Windows SDK）
signtool verify /pa /v CodeMaker-Hub-Light_0.2.0_x64-setup.exe

# 3) 对照 Release 说明里的 SHA-256
Get-FileHash .\CodeMaker-Hub-Light_0.2.0_x64-setup.exe -Algorithm SHA256
```

期望结果（签名生效后）：`Status = Valid`，签名者证书主体含 `SignPath Foundation`，且带可信时间戳。未签名时 `Status = NotSigned`。

## 10. 申请与接入清单

| 条件 | 状态 | 备注 |
|---|---|---|
| OSI 许可证 | ✅ | GPL-3.0-only |
| 无专有组件 | ✅ | 全部自研，依赖均为开源 |
| 项目已发布 | ✅ | `v0.1.0`、`v0.2.0` Release 与 tag |
| 有文档 | ✅ | `docs/` 十篇 + README |
| 代码签名政策页 | ✅ | 本文 + README 的 Code signing policy 段 |
| 隐私政策 | ✅ | 本文第 7 节 |
| 仓库公开 | ✅ | `CMHubLight/codemaker-hub-light` 已公开 |
| GitHub / SignPath 双端 MFA | ⏳ | GitHub 侧 ✅ 已开启；SignPath 注册后开启 |
| 安装 SignPath GitHub App | ⏳ | 授权仓库读权限 |
| SignPath 项目与签名策略 | ⏳ | 项目 slug 建议 `codemaker-hub-light`，策略建议 `release-signing` |
| Artifact Configuration | ⏳ | 导入本文第 6 节模板，slug 建议 `nsis-installer` |
| API Token 与仓库 Secrets/Variables | ⏳ | 见 [开发与构建](development.md#9-发布与签名) |
| 每版人工批准 | ✅ | 流程已固定在第 5 节 |

提交申请：<https://signpath.org/apply>（用 CMHubLight 的仓库地址与维护者身份）。

## 11. 申请表单信息

提交申请时使用下列值（仓库与维护者已从旧账号迁移到 `CMHubLight`）：

| 字段 | 值 |
|---|---|
| Project name | CodeMaker Hub Light |
| Repository | <https://github.com/CMHubLight/codemaker-hub-light> |
| License | GPL-3.0-only |
| Maintainer | [@CMHubLight](https://github.com/CMHubLight) |
| Project type | Open-source Windows desktop application (Tauri 2 + Rust + React 19) |
| Description (EN) | A Windows desktop app that signs in to a user-configured compatible gateway, exposes a local HTTP proxy (127.0.0.1) for Claude Code, Codex and OpenAI/Anthropic-compatible clients, and records per-request token and cost usage locally. No telemetry, no bundled service endpoints. |
| Latest release | `v0.2.0`（安装包当前未签名） |
| Build system | GitHub Actions，GitHub 托管 `windows-latest`，`.github/workflows/release-sign.yml` |
| Code signing policy | <https://github.com/CMHubLight/codemaker-hub-light/blob/main/docs/code-signing-policy.md> |
| Roles | 单人维护：Author / Reviewer / Approver 均为 @CMHubLight（需先开启 MFA） |
| Signing target | NSIS 安装包 `.exe`（Authenticode，SHA-256 + RFC 3161 时间戳） |
| Artifact configuration | 仓库内 `.github/signpath/artifact-configuration.xml`（slug 建议 `nsis-installer`） |
| Privacy | 见本文第 7 节：无遥测、不向维护者或第三方回传数据 |
| Uninstall / system changes | 见本文第 8 节 |

> 申请前逐项确认：仓库已 public、CMHubLight 已开启两步验证、SignPath 账号已开启 MFA、GPG/签名令牌不进入仓库。

## 12. 源码索引

- `.github/workflows/release-sign.yml`
- `.github/signpath/artifact-configuration.xml`
- `README.md`
- `README.zh-CN.md`
- `docs/development.md`
- `src-tauri/tauri.conf.json`
- `package.json`
