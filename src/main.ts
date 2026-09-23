// 前端入口：创建 Vue 应用，把根组件 App.vue 挂到 index.html 的 #app 节点上。
// 所有界面逻辑都从 App.vue 开始，本文件只负责“点火”。
import { createApp } from "vue";
import App from "./App.vue";
import "./style.css"; // 全局样式（主题色、滚动条、通用按钮等）

// Windows 宿主标记：style.css 里 html.win32 body 恢复 ClearType 次像素渲染，
// 小字号中文/数字比 macOS 习惯的灰度抗锯齿更锐利（原因见 style.css 文件头）。
if (navigator.platform.toLowerCase().includes("win")) {
    document.documentElement.classList.add("win32");
}

createApp(App).mount("#app");
