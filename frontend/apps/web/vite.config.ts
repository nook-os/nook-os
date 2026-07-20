import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// /api and /docs are proxied to the control plane so the browser sees a single
// origin — this keeps the session cookie SameSite-simple in dev.
const apiTarget = process.env.NOOK_API_PROXY ?? "http://localhost:8080";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    // Poll for changes, for the same reason cargo watch does (see the
    // control-plane command in docker-compose.yml): inotify events don't
    // cross the bind mount into the container, so a saved file is simply
    // never seen. Without this the dev server serves the code it started
    // with, forever, and looks perfectly healthy doing it.
    watch: { usePolling: true, interval: 300 },
    proxy: {
      "/api": {
        target: apiTarget,
        changeOrigin: true,
        ws: true,
        // changeOrigin rewrites Host to the container, which is how the
        // control plane would otherwise generate install commands pointing at
        // "http://control-plane:8080" — unreachable from any real machine.
        // Tell it how the browser actually got here, same as a prod proxy.
        headers: { "x-forwarded-host": "localhost:5173", "x-forwarded-proto": "http" },
      },
      "/docs": { target: apiTarget, changeOrigin: true },
      "/openapi.json": { target: apiTarget, changeOrigin: true },
      "/mcp": { target: apiTarget, changeOrigin: true },
    },
  },
});
