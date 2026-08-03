import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// /api and /docs are proxied to the control plane so the browser sees a single
// origin — this keeps the session cookie SameSite-simple in dev.
const apiTarget = process.env.NOOK_API_PROXY ?? "http://localhost:8080";
// Team chat is its own service (MAIN-48); /chat is proxied to it the same way,
// with the prefix stripped since nook-chat serves its routes at the root.
const chatTarget = process.env.NOOK_CHAT_PROXY ?? "http://localhost:8082";

// The dev server's own port. Read, never hardcoded (MAIN-376): nook leases a
// number per declared listener and exports it as the variable `.nook.toml`
// names, which is what lets two worktrees of this repo run at once instead of
// both grabbing 5173. Unset — a plain clone, no nook — falls back to 5173, so
// nothing outside a session changes.
const webPort = Number(process.env.NOOK_WEB_PORT) || 5173;

// What the BROWSER used to get here, which is not always what we bind.
//
// In compose the two genuinely differ: the mapping is `<leased>:5173`, so Vite
// must keep binding 5173 INSIDE the container while the browser arrives on the
// leased host port. Reporting the bind port there is not a cosmetic error —
// `request_base` (routes/dist.rs) builds the `nook join` install command from
// this header, so on a machine running two checkouts it hands you a command
// pointing at the OTHER instance.
//
// Defaults to the bind port, which is the truth everywhere the two are the same
// (a plain `pnpm dev`, and any non-container run).
const publicPort = Number(process.env.NOOK_WEB_PUBLIC_PORT) || webPort;

export default defineConfig({
  plugins: [react()],
  server: {
    port: webPort,
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
        headers: { "x-forwarded-host": `localhost:${publicPort}`, "x-forwarded-proto": "http" },
      },
      // PROXY API PREFIXES, NEVER PAGE ROUTES (MAIN-173). `/chat` is also the
      // SPA's Chat route, so a proxy on the bare prefix swallowed the browser's
      // document request and every visit or refresh of /chat 404'd from the
      // chat service. The API actually lives under `/chat/api` (see
      // `CHAT_PREFIX`), websocket included, so that is what is forwarded — and
      // `/chat` itself falls through to the SPA shell. The prod nginx is
      // narrowed identically, in the same commit, so the two cannot drift.
      "/chat/api": {
        target: chatTarget,
        changeOrigin: true,
        ws: true,
        // nook-chat serves its routes at the root, so drop the `/chat` prefix
        // before forwarding: `/chat/api/me` → `/api/me`.
        rewrite: (path) => path.replace(/^\/chat/, ""),
        headers: { "x-forwarded-host": `localhost:${publicPort}`, "x-forwarded-proto": "http" },
      },
      "/docs": { target: apiTarget, changeOrigin: true },
      "/openapi.json": { target: apiTarget, changeOrigin: true },
      "/mcp": { target: apiTarget, changeOrigin: true },
    },
  },
});
