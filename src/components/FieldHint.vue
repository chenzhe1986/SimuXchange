<script setup lang="ts">
// FieldHint.vue —— 字段名旁边的"要导"说明图标。
// 鼠标悬停（:hover）或点击（获得焦点 :focus）时在图标下方弹跳说明文字，
// 移开 / 失焦后自动隐藏，纯 CSS 实现无需 JS 状态；tabindex 让键盘 Tab
// 也能聚焦查看，兼顾当正访问与触屏点击。
// right=true 时弹层右对齐（左对齐适合靠左字段，两列网格中靠右的字段
// 用 right=true 避免弹层左缘浮出320px以弹窗边框截断）。
defineProps<{ text: string; right?: boolean }>();
</script>

<template>
    <span
        class="field-hint"
        :class="{ 'align-right': right }"
        tabindex="0"
        role="tooltip"
        :aria-label="text"
    >
        <span class="field-hint-icon">?</span>
        <span class="field-hint-popup">{{ text }}</span>
    </span>
</template>

<style scoped>
.field-hint {
    position: relative;
    display: inline-flex;
    align-items: center;
    vertical-align: middle; /* 放进 section-title 等行内标题时与文字垂直居中 */
    cursor: help;
    outline: none; /* 聚焦态用弹窗展示，不显示默认焦点环 */
}

/* 问号圆点：细边框小圆 + 灰色问号，尺寸与 checkbox 文字对齐 */
.field-hint-icon {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 15px;
    height: 15px;
    margin-left: 5px;
    border-radius: 50%;
    border: 1px solid var(--text-faint);
    color: var(--text-faint);
    font-size: 12px;
    line-height: 1;
    user-select: none;
}

.field-hint:hover .field-hint-icon,
.field-hint:focus .field-hint-icon {
    border-color: var(--accent);
    color: var(--accent);
}

/* 弹出层：定位于图标正下方、从图标左缘向右展开（居中定位在字段靠左时会
   左移越界被弹窗边框裁剪）；opacity + transform 过渡出现。
   visibility 与 opacity 同时控制，避免隐藏时仍拦截点击 */
.field-hint-popup {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    transform: translateY(-4px);
    width: 240px;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--panel);
    box-shadow: 0 4px 14px rgba(0, 0, 0, 0.16);
    font-size: 12px;
    line-height: 1.55;
    color: var(--text-dim);
    text-align: left;
    visibility: hidden;
    opacity: 0;
    transition: opacity 0.15s ease, transform 0.15s ease;
    z-index: 30;
    pointer-events: none;
}

.field-hint:hover .field-hint-popup,
.field-hint:focus .field-hint-popup {
    visibility: visible;
    opacity: 1;
    transform: translateY(0);
}

/* 靠右字段的弹层右对齐：以图标右缘为锚向左展开，避免超出表格右边界被裁剪 */
.field-hint.align-right .field-hint-popup {
    left: auto;
    right: 0;
}
</style>