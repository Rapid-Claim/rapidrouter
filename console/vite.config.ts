import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  base: "/console/",
  plugins: [solid()],
  build: {
    target: "es2022",
    sourcemap: false,
    cssCodeSplit: false,
  },
});
