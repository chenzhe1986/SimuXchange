import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Tauri 前端开发服务器配置
export default defineConfig({
    plugins: [vue()],
    clearScreen: false,
    server: {
        // 注意：Windows 上 1381–1480 等端口段被系统保留（Hyper-V/WSL），
        // 需选用保留段之外的端口，并与 tauri.conf.json 的 devUrl 保持一致
        host: "127.0.0.1",
        port: 14200,
        strictPort: true,
    },
    envPrefix: ["VITE_", "TAURI_"],
    build: {
        target: "chrome105",
        minify: "esbuild",
    },
});
