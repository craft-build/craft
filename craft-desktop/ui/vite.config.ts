import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri's recommended dev-server setup: fixed port matching tauri.conf.json's
// devUrl, strictPort so a stale process fails loudly instead of silently
// binding elsewhere, and ignoring the Rust crate from the watcher.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**", "**/target/**"],
    },
  },
});
