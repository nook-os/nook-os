import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// /api and /docs are proxied to the control plane so the browser sees a single
// origin — this keeps the session cookie SameSite-simple in dev.
const apiTarget = process.env.NOOK_API_PROXY ?? "http://localhost:8080";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: apiTarget,
        changeOrigin: true,
        ws: true,
      },
      "/docs": { target: apiTarget, changeOrigin: true },
      "/openapi.json": { target: apiTarget, changeOrigin: true },
      "/mcp": { target: apiTarget, changeOrigin: true },
    },
  },
});
