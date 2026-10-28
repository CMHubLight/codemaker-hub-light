// ESLint 9 flat config：只覆盖前端源码与构建脚本。
// 目标不是风格（本项目不做 Prettier），而是抓真实缺陷：未使用变量、错误的 hook 依赖、隐式 any。
import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import tseslint from 'typescript-eslint'

export default tseslint.config(
  {
    // src-tauri 不参与前端 lint；dist 是构建产物。
    // research/ 是本地保留的调研目录（已在 .gitignore 排除，不随仓库分发）。
    ignores: ['dist/**', 'node_modules/**', 'src-tauri/**', 'research/**']
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ['src/**/*.{ts,tsx}'],
    languageOptions: {
      ecmaVersion: 2022,
      globals: { ...globals.browser },
      parserOptions: { ecmaFeatures: { jsx: true } }
    },
    plugins: { 'react-hooks': reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      // 允许以 _ 开头的占位参数/变量（测试 helper 常用）。
      '@typescript-eslint/no-unused-vars': [
        'warn',
        { argsIgnorePattern: '^_', varsIgnorePattern: '^_', caughtErrorsIgnorePattern: '^_' }
      ],
      '@typescript-eslint/no-explicit-any': 'warn',
      // release 无控制台；console.error/warn 是错误边界与开发期唯一出口。
      'no-console': ['warn', { allow: ['error', 'warn'] }]
    }
  },
  {
    files: ['src/**/*.test.{ts,tsx}', 'src/test/**/*.ts'],
    languageOptions: { globals: { ...globals.browser, ...globals.node } }
  },
  {
    files: ['*.config.{js,ts}', 'vitest.config.ts'],
    languageOptions: { globals: { ...globals.node } }
  },
  {
    // 门禁脚本是 Node ESM，需要 node 全局（console/process/Buffer/require）。
    files: ['scripts/**/*.mjs'],
    languageOptions: { globals: { ...globals.node } }
  }
)
