import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  // ES-module workers (MapLibre 6's tile worker imports a shared chunk).
  worker: { format: 'es' },
  // Shown in Settings → About, so a stale bundle is recognisable.
  define: { __BUILD_TIME__: JSON.stringify(new Date().toISOString()) },
  server: {
    port: 5173,
    // Proxy to the brain so the browser never makes cross-origin requests.
    proxy: {
      '/mcp':               'http://localhost:3001',
      '/chat':              'http://localhost:3001',
      '/todos':             'http://localhost:3001',
      '/scheduled-tasks':   'http://localhost:3001',
      '/api':               'http://localhost:3001',
      // Self-hosted map tiles, served by the HBI container (MAP_PLAN).
      '/maps':              'http://localhost:3002',
    },
    allowedHosts: [
      'pcman', 
      'pcmain.local', 
      'pcmain.daggertooth-city.ts.net'
    ]
  },
})
