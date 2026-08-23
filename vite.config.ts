import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import { resolve } from "node:path";

export default defineConfig({
  plugins: [react()],
  root: ".",
  build: {
    outDir: "dist/client",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: resolve("index.html"),
        technicallySpeaking: resolve("technically-speaking/index.html")
      }
    }
  },
  server: {
    host: "127.0.0.1"
  }
});
