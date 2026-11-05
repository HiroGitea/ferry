import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// In development the API is proxied to a locally running mlqd.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: {
      "/api": { target: process.env.MLQ_URL ?? "http://127.0.0.1:7070", changeOrigin: true },
      "/mcp": { target: process.env.MLQ_URL ?? "http://127.0.0.1:7070", changeOrigin: true },
    },
  },
  build: { outDir: "dist", emptyOutDir: true, sourcemap: false },
});
