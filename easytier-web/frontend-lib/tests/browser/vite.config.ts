import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import vue from '@vitejs/plugin-vue'
import ViteYaml from '@modyfi/vite-plugin-yaml'
import { defineConfig } from 'vite'

const testDir = dirname(fileURLToPath(import.meta.url))

export default defineConfig({
  root: resolve(testDir, '../..'),
  plugins: [vue(), ViteYaml()],
  server: {
    host: '127.0.0.1',
  },
})
