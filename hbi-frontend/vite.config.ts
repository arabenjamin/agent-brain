import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    // Proxy to the brain so the browser never makes cross-origin requests.
    proxy: {
      '/mcp':  'http://localhost:3001',
      '/chat': 'http://localhost:3001',
    },
    allowedHosts: [
      'pcman', 
      'pcmain.local', 
      'pcmain.daggertooth-city.ts.net'
    ]
  },
})
