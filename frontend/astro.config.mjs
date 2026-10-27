// @ts-check
import { defineConfig } from 'astro/config';

// 开发期 dev server 将 /api 与会话 cookie 代理到本地 unidns 服务
export default defineConfig({
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:18686',
    },
  },
  build: {
    inlineStylesheets: 'auto',
  },
});
