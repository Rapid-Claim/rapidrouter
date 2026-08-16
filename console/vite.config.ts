import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Where `npm run dev` sends everything that is not a console asset. The
// dev server has no gateway in it: the admin API, the proxy endpoints and
// the SSE event stream all belong to the running binary, and without this
// the console loads and then fails every request it makes.
const GATEWAY = process.env.CARET_GATEWAY ?? "http://127.0.0.1:8080";

export default defineConfig({
  base: "/console/",
  plugins: [solid()],
  server: {
    proxy: {
      // `ws: false` on the event stream is deliberate — it is SSE over
      // plain HTTP, and asking Vite to treat it as a websocket upgrade
      // makes the console show "Reconnecting" forever.
      "/admin": { target: GATEWAY, changeOrigin: true, ws: false },
      "/v1": { target: GATEWAY, changeOrigin: true },
      "/health": { target: GATEWAY, changeOrigin: true },
      "/metrics": { target: GATEWAY, changeOrigin: true },
    },
  },
  build: {
    target: "es2022",
    sourcemap: false,
    cssCodeSplit: false,
  },
});
