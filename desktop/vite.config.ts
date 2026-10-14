import { fileURLToPath, URL } from 'node:url'

import tailwindcss from '@tailwindcss/vite'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

// Set by `tauri dev` when the host is a phone or another machine.
const host = process.env.TAURI_DEV_HOST

const dev = process.env.TAURI_DEBUG === 'true'

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  // Tauri owns the terminal while it runs, so do not clear it.
  clearScreen: false,
  server: {
    // Tauri expects a fixed port and fails rather than picking another.
    port: 1420,
    strictPort: true,
    host: host ?? false,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: {
      ignored: ['**/src-tauri/**'],
    },
  },
  // `TAURI_` vars are consumed by the shell at build time.
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    // The bundled webviews are older than the newest browsers.
    target: ['es2021', 'chrome100', 'safari13'],
    // Let Vite pick its own minifier; naming one here couples us to it.
    minify: !dev,
    sourcemap: dev,
  },
})
