// 前端入口：创建 Vue 应用，把根组件 App.vue 挂到 index.html 的 #app 节点上。
// 所有界面逻辑都从 App.vue 开始，本文件只负责“点火”。
import { createApp } from "vue";
import App from "./App.vue";
import "./style.css"; // 全局样式（主题色、滚动条、通用按钮等）

createApp(App).mount("#app");
