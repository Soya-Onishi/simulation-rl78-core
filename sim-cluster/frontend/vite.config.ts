import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

// https://vite.dev/config/
export default defineConfig({
  base: '/',
  plugins: [react()],
  build: {
    // sim-cluster/out/web (relative to this directory)
    outDir: '../out/web',
    emptyOutDir: true,
  },
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
})
