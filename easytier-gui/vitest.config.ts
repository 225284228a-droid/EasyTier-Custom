import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vitest/config'

// These bridge/composable tests run without the production page scanner and
// development-tools plugins, which are unnecessary in a Node test process.
export default defineConfig({
  resolve: { alias: { '~': fileURLToPath(new URL('./src', import.meta.url)) } },
  test: { environment: 'node', include: ['src/**/*.test.ts'] },
})
