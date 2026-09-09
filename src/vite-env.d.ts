/// <reference types="vite/client" />

declare module "*.vue" {
    import type { DefineComponent } from "vue";
    const component: DefineComponent<Record<string, never>, Record<string, never>, unknown>;
    export default component;
}

// Vite 的 ?raw 后缀：把任意文件以原始字符串打进 bundle（构建时内联，
// 运行时无需再读文件）——"脚本手册"弹窗内嵌 docs/ 下说明文档用
declare module "*.md?raw" {
    const content: string;
    export default content;
}
