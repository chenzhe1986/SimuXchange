<script setup lang="ts">
// LuaEditor.vue —— Lua 回报脚本编辑器（CodeMirror 6 封装）。
//
// 只做一件事：给平台编辑器提供一个带 Lua 语法高亮的多行代码编辑框，
// v-model 双向绑定脚本内容；配色全部引用 style.css 的主题变量，
// 五套主题（深/浅）下自动跟随切换，无需单独适配。
//
// 为什么不用普通 textarea：需求要求 Lua 语法高亮 + 行号（脚本报错
// 提示带行号，行号对用户有直接意义）。CodeMirror 6 是模块化的编辑器
// 内核（类比把编辑器拆成一堆小库按需拼装），这里按需装配：
// 行号 / 撤销历史 / Lua 语法流式解析（legacy-modes 里的 lua mode）/ 配色。
//
// 双向绑定的实现要点：CodeMirror 的 EditorView 内部持有自己的文档
// 状态（外部不可直接改）。所以两个方向分别处理——
// - 编辑器内输入：updateListener 监听文档变化，把全文 emit 给父组件；
// - 外部替换（切换平台 / 预填模板）：watch 里比对后用 dispatch 整体
//   替换文档（不比对直接替换会导致每次 emit 后光标被重置到开头）。
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { HighlightStyle, StreamLanguage, syntaxHighlighting } from "@codemirror/language";
import { lua } from "@codemirror/legacy-modes/mode/lua";
import { tags as t } from "@lezer/highlight";

const props = defineProps<{
    /** 脚本内容（v-model） */
    modelValue: string;
    /** 编辑器高度（像素，默认 220） */
    height?: number;
}>();

const emit = defineEmits<{
    (e: "update:modelValue", v: string): void;
}>();

// 编辑器挂载点：ref 拿到模板里的 div，onMounted 后传给 CodeMirror 当 parent
const host = ref<HTMLDivElement | null>(null);
// EditorView 实例不放进 ref：它不是响应式数据，只是命令式对象
// （类比 C++ 里不属于 UI 数据绑定层的原生句柄）
let view: EditorView | null = null;

// 语法高亮配色：全部引用全局 CSS 变量，主题切换即时生效
const luaHighlight = HighlightStyle.define([
    { tag: t.keyword, color: "var(--accent)", fontWeight: "600" }, // if/then/return/end…
    { tag: t.comment, color: "var(--text-faint)", fontStyle: "italic" }, // 注释
    { tag: [t.string, t.special(t.string)], color: "var(--green)" }, // "confirm"…
    { tag: [t.number, t.bool, t.null], color: "var(--amber)" }, // 数字/true/false
    { tag: [t.operator, t.punctuation, t.bracket], color: "var(--text-dim)" },
    { tag: t.function(t.variableName), color: "var(--accent)" }, // math.floor 等调用
]);

onMounted(() => {
    view = new EditorView({
        parent: host.value!,
        state: EditorState.create({
            doc: props.modelValue,
            extensions: [
                lineNumbers(),
                history(),
                // defaultKeymap：光标移动/选中等基础键位；indentWithTab：
                // Tab 缩进（代码编辑器惯例，编辑器内不参与弹窗焦点切换）
                keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
                StreamLanguage.define(lua),
                syntaxHighlighting(luaHighlight),
                EditorView.lineWrapping,
                // 编辑器外观：贴全局输入框观感（--mono 等宽字体、主题底色）
                EditorView.theme({
                    "&": { fontSize: "12px", height: `${props.height ?? 220}px` },
                    ".cm-scroller": { fontFamily: "var(--mono)", lineHeight: "1.55" },
                    ".cm-content": { padding: "6px 0" },
                    ".cm-gutters": {
                        background: "transparent",
                        border: "none",
                        color: "var(--text-faint)",
                    },
                    ".cm-activeLine": { background: "transparent" },
                    ".cm-activeLineGutter": { background: "transparent", color: "var(--text-dim)" },
                    "&.cm-focused": { outline: "none" },
                    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--accent)" },
                    ".cm-selectionBackground": { backgroundColor: "var(--accent-soft) !important" },
                }),
                EditorView.updateListener.of((u) => {
                    if (u.docChanged) emit("update:modelValue", u.state.doc.toString());
                }),
            ],
        }),
    });
});

// 外部替换脚本内容（切换平台 / 空脚本预填模板）时同步进编辑器
watch(
    () => props.modelValue,
    (v) => {
        if (view && v !== view.state.doc.toString()) {
            view.dispatch({
                changes: { from: 0, to: view.state.doc.length, insert: v },
            });
        }
    },
);

onBeforeUnmount(() => view?.destroy());
</script>

<template>
    <div ref="host" class="lua-editor"></div>
</template>

<style scoped>
/* 外框贴合全局输入框观感：--bg-soft 底 + 边框，聚焦时描边变主色 */
.lua-editor {
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--bg-soft);
    overflow: hidden;
    text-align: left;
}
.lua-editor:focus-within {
    border-color: var(--accent);
}
</style>
