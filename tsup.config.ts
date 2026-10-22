import { defineConfig } from 'tsup'

export default defineConfig({
  entry: {
    index: 'src/index.ts',
    'pi-mcp-extension': 'src/mcp/pi-mcp-extension.ts'
  },
  format: ['esm'],
  platform: 'node',
  target: 'node22',
  sourcemap: true,
  clean: true,
  dts: false,
  splitting: false,
  minify: false,
  banner: {
    js: '#!/usr/bin/env node'
  }
})
