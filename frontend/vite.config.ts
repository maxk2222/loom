import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const variant = process.env.VITE_APP_VARIANT ?? "server";
const outDir = variant === "client" ? "dist-client" : "dist";
const proxyTarget = process.env.LOOM_DEV_API ?? "http://localhost:8080";

export default defineConfig({
  base: "/ui/",
  plugins: [react()],
  define: {
    __VARIANT__: JSON.stringify(variant),
  },
  build: {
    outDir,
    emptyOutDir: true,
    sourcemap: false,
    rollupOptions: {
      output: {
        manualChunks: {
          vendor: ["react", "react-dom", "react-router-dom", "@tanstack/react-query"],
          charts: ["recharts"],
        },
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: proxyTarget, changeOrigin: true },
      "/v1": { target: proxyTarget, changeOrigin: true },
      "/local/api": { target: proxyTarget, changeOrigin: true },
    },
  },
});
