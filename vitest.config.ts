import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'

// 前端测试配置与构建配置分离：
// - 默认 node 环境（纯逻辑测试，如 ipc/format）；
// - 组件测试文件顶部加 `// @vitest-environment jsdom` 自行声明 DOM 环境，
//   避免给全部测试引入 jsdom 启动开销。
export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'node',
    include: ['src/**/*.test.{ts,tsx}'],
    setupFiles: ['./src/test/setup.ts'],
    restoreMocks: true
  }
})
