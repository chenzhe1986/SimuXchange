<script setup lang="ts">
// LuaDocViewer.vue —— "Lua 脚本回报编写说明"文档弹窗（PlatformEditor 的
// 「脚本手册」按钮触发）。
//
// 文档单一来源：用 Vite 的 ?raw 后缀把 docs/ 下的 Markdown 原文以内嵌字符串
// 方式打进前端 bundle（构建时内联，运行时零 IO；文档更新后重新构建即自动
// 同步进程序，不需要维护第二份拷贝）。
// 渲染用 marked（轻量 Markdown → HTML 转换器）：文档是我们自己维护的静态
// 内容、不含用户输入，直接 v-html 无注入风险，无需再挂 DOMPurify。
import { computed } from "vue";
import { marked } from "marked";
import docRaw from "../../docs/Lua脚本回报编写说明.md?raw";

// 两个出口：关闭（点遮罩 / ✕；Esc 由 App 的全局按键统一处理）
const emit = defineEmits<{
    (e: "close"): void;
}>();

// Markdown → HTML（同步转换）
const html = computed(() => marked.parse(docRaw) as string);
</script>

<template>
    <!-- 叠在平台编辑弹窗之上的大号文档弹窗（.modal 最大高度 88vh，正文区滚动） -->
    <div class="modal-mask" @mousedown.self="emit('close')">
        <div class="modal" style="width: 780px">
            <div class="modal-head">
                <span>Lua 脚本回报编写说明</span>
                <button class="modal-close" @click="emit('close')">✕</button>
            </div>
            <!-- v-html：渲染自家维护的静态文档，无用户输入，无注入风险 -->
            <div class="modal-body md-body" v-html="html"></div>
        </div>
    </div>
</template>

<style scoped>
/* Markdown 基础排版：全部引用主题变量，五套配色下自动跟随 */
.md-body {
    line-height: 1.75;
    font-size: 13px;
    color: var(--text);
}
.md-body :deep(h1),
.md-body :deep(h2),
.md-body :deep(h3),
.md-body :deep(h4) {
    color: var(--accent);
    margin: 18px 0 8px;
    line-height: 1.4;
}
.md-body :deep(h1) {
    font-size: 20px;
}
.md-body :deep(h2) {
    font-size: 17px;
    border-bottom: 1px solid var(--border);
    padding-bottom: 4px;
}
.md-body :deep(h3) {
    font-size: 15px;
}
.md-body :deep(h4) {
    font-size: 13.5px;
}
.md-body :deep(p) {
    margin: 8px 0;
}
.md-body :deep(ul),
.md-body :deep(ol) {
    margin: 8px 0;
    padding-left: 22px;
}
.md-body :deep(li) {
    margin: 3px 0;
}
.md-body :deep(a) {
    color: var(--accent);
}
.md-body :deep(strong) {
    color: var(--text);
}
.md-body :deep(code) {
    font-family: var(--mono);
    font-size: 12px;
    background: var(--bg-soft);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 1px 5px;
}
.md-body :deep(pre) {
    background: var(--bg-soft);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 10px 12px;
    overflow-x: auto;
    margin: 10px 0;
}
.md-body :deep(pre code) {
    background: none;
    border: none;
    padding: 0;
    line-height: 1.6;
}
.md-body :deep(table) {
    border-collapse: collapse;
    margin: 10px 0;
    width: 100%;
    font-size: 12.5px;
}
.md-body :deep(th),
.md-body :deep(td) {
    border: 1px solid var(--border);
    padding: 5px 9px;
    text-align: left;
}
.md-body :deep(th) {
    background: var(--bg-soft);
    color: var(--accent);
    font-weight: 600;
}
.md-body :deep(blockquote) {
    border-left: 3px solid var(--accent);
    background: var(--bg-soft);
    margin: 10px 0;
    padding: 8px 14px;
    color: var(--text-dim);
    border-radius: 0 var(--radius-sm) var(--radius-sm) 0;
}
.md-body :deep(hr) {
    border: none;
    border-top: 1px dashed var(--border);
    margin: 16px 0;
}
</style>
