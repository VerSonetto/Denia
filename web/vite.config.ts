import { defineConfig, loadEnv } from 'vite'

/**
 * 开发服务器:控制台用相对路径 /api 访问服务端。
 * 生产环境由服务端同源提供,开发时若不代理,/api 会落到 Vite 的 SPA 回退返回 index.html。
 * 端口不固定时用 DENIA_API_URL 覆盖。
 */
export default defineConfig(({ mode }) => ({
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: loadEnv(mode, '.', 'DENIA_').DENIA_API_URL || 'http://127.0.0.1:43654',
        changeOrigin: true,
        ws: true,
      },
    },
  },
}))
