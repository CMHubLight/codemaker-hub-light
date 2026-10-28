import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// 前端只跑在 Tauri WebView（或本机浏览器预览）里：
// - 端口固定 5173，与 src-tauri/tauri.conf.json 的 devUrl 对齐；被占用时直接失败，避免后端连错端口。
// - 不显式设置 host：沿用 Vite 默认的 localhost（仅本机回环），与 devUrl 的 localhost 完全一致。
// - src-tauri/ 不参与前端热更新。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  envPrefix: ['VITE_', 'TAURI_'],
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      ignored: ['**/src-tauri/**']
    }
  },
  build: {
    // Tauri 2 在 Windows 上使用 WebView2（Chromium 内核），基线 chrome105 覆盖 Win10/11。
    target: 'chrome105',
    minify: 'esbuild',
    sourcemap: false
  }
})
