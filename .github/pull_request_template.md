## 变更说明

<!-- 这个 PR 做了什么、为什么 -->

## 验证方式

<!-- 跑了哪些命令 / 手工验证步骤；贴关键输出 -->

- [ ] `pnpm verify`（含 `docs:check`）
- [ ] `cargo fmt --manifest-path src-tauri/Cargo.toml --check`
- [ ] `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml`

## 文档

- [ ] 变更了用户可见行为（接口、设置键、状态码、数据表），并已同步更新 `docs/` 对应文档
- [ ] 没有在仓库里新增任何内网地址、账号、密钥或本机绝对路径

## 关联 issue

<!-- 例如 Closes #12 -->
