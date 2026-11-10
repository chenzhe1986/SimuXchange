<script setup lang="ts">
// FieldHint.vue —— 字段名旁边的“?”说明图标。
// 鼠标悬停（:hover）或键盘聚焦（:focus）时在图标附近弹出说明文字。
//
// 早期版本把气泡绝对定位在图标内部，会被弹窗的滚动容器（.modal-body 的
// overflow:auto）裁剪：靠近底部的字段气泡向下超出被截断，右对齐气泡在左栏
// 还会向左伸出弹窗。现在改为把气泡 Teleport 到 <body>、用 fixed 定位，
// 由 JS 读取图标与气泡尺寸后计算坐标：下方放不下翻到上方、水平方向自动
// 换边并夹在视口内，因此无论字段在弹窗什么位置都能完整显示。
// 采用 Teleport 而不是仍放在原位：除了规避 overflow 裁剪，也避开弹窗入场
// 动画的 transform（transform 祖先会让 fixed 定位相对祖先而非视口）。
import { nextTick, onBeforeUnmount, ref } from "vue";

const props = defineProps<{ text: string; right?: boolean }>();

/** 问号图标的 DOM（定位锚点） */
const anchorEl = ref<HTMLElement | null>(null);
/** 弹出层的 DOM（Teleport 到 body；常驻 DOM 仅切换可见性，便于随时测量宽高） */
const popupEl = ref<HTMLElement | null>(null);
/** 气泡是否可见 */
const show = ref(false);
/** 气泡的 fixed 定位坐标（相对视口） */
const pos = ref({ left: 0, top: 0 });

/** 悬停 / 聚焦状态：两者任一成立就保持显示（与纯 CSS 的 :hover/:focus 语义一致） */
const hovered = ref(false);
const focused = ref(false);
/** 是否已挂上滚动/缩放监听（避免 hover 与 focus 重复 addEventListener） */
let listening = false;

const GAP = 6; // 气泡与图标的间距
const MARGIN = 8; // 气泡与视口边缘的最小距离

/** 气泡水平坐标：优先按 right 指定方向对齐；若该方向会超出最近的
 *  .modal-body（没有弹窗时用视口），改用另一侧，尽量让气泡完整落在
 *  弹窗内容区内；最后再夹进视口，保证任何情况下都不会被裁掉 */
function placeX(anchor: DOMRect, w: number): number {
    const box = anchorEl.value?.closest(".modal-body")?.getBoundingClientRect();
    const min = (box ? box.left : 0) + MARGIN;
    const max = (box ? box.right : window.innerWidth) - MARGIN;
    const leftAligned = anchor.left;
    const rightAligned = anchor.right - w;
    const fits = (x: number) => x >= min && x + w <= max;
    const preferred = props.right ? rightAligned : leftAligned;
    const other = props.right ? leftAligned : rightAligned;
    const x = fits(preferred) ? preferred : fits(other) ? other : preferred;
    return Math.min(Math.max(MARGIN, x), window.innerWidth - MARGIN - w);
}

/** 气泡垂直坐标：优先显示在图标下方；下方空间不足（以最近的 .modal-body
 *  为界，避免盖住弹窗底部按钮区）时翻到上方；上下都放不下则选空间更大
 *  的一侧并夹进视口 */
function placeY(anchor: DOMRect, h: number): number {
    const box = anchorEl.value?.closest(".modal-body")?.getBoundingClientRect();
    const min = (box ? box.top : 0) + MARGIN;
    const max = (box ? box.bottom : window.innerHeight) - MARGIN;
    const below = anchor.bottom + GAP;
    const above = anchor.top - GAP - h;
    if (below + h <= max) return below;
    if (above >= min) return above;
    const preferAbove = anchor.top - min > max - anchor.bottom;
    const y = preferAbove ? above : below;
    return Math.max(MARGIN, Math.min(y, window.innerHeight - MARGIN - h));
}

/** 依据图标当前位置与气泡实际尺寸重新定位 */
function place() {
    const anchor = anchorEl.value?.getBoundingClientRect();
    const popup = popupEl.value;
    if (!anchor || !popup) return;
    pos.value = {
        left: placeX(anchor, popup.offsetWidth),
        top: placeY(anchor, popup.offsetHeight),
    };
}

/** 滚动（含弹窗内部滚动）或窗口缩放时跟随锚点重新定位 */
function onViewportChange() {
    if (show.value) place();
}

function startListen() {
    if (listening) return;
    listening = true;
    // 捕获阶段监听：任意滚动容器（如 .modal-body）滚动都能收到
    window.addEventListener("scroll", onViewportChange, true);
    window.addEventListener("resize", onViewportChange);
}

function stopListen() {
    if (!listening) return;
    listening = false;
    window.removeEventListener("scroll", onViewportChange, true);
    window.removeEventListener("resize", onViewportChange);
}

/** 同步显隐：悬停或聚焦任一成立即显示 */
async function sync() {
    const want = hovered.value || focused.value;
    if (want === show.value) return;
    show.value = want;
    if (want) {
        // 气泡常驻 DOM（visibility:hidden 也有布局尺寸），可先测量再定位；
        // 等一次渲染让内容变化先落到 DOM 上
        await nextTick();
        place();
        startListen();
    } else {
        stopListen();
    }
}

function onEnter() {
    hovered.value = true;
    void sync();
}

function onLeave() {
    hovered.value = false;
    void sync();
}

function onFocus() {
    focused.value = true;
    void sync();
}

function onBlur() {
    focused.value = false;
    void sync();
}

onBeforeUnmount(stopListen);
</script>

<template>
    <span
        ref="anchorEl"
        class="field-hint"
        tabindex="0"
        role="tooltip"
        :aria-label="text"
        @mouseenter="onEnter"
        @mouseleave="onLeave"
        @focus="onFocus"
        @blur="onBlur"
    >
        <span class="field-hint-icon">?</span>
        <Teleport to="body">
            <span
                ref="popupEl"
                class="field-hint-popup"
                :class="{ visible: show }"
                :style="{ left: pos.left + 'px', top: pos.top + 'px' }"
            >{{ text }}</span>
        </Teleport>
    </span>
</template>

<style scoped>
.field-hint {
    position: relative;
    display: inline-flex;
    align-items: center;
    vertical-align: middle; /* 放进 section-title 等行内标题时与文字垂直居中 */
    flex-shrink: 0; /* 窄栏位里不被 flex 压缩，避免问号圆被挤扁 */
    cursor: help;
    outline: none; /* 聚焦态用气泡展示，不显示默认焦点环 */
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
    flex-shrink: 0;
}

.field-hint:hover .field-hint-icon,
.field-hint:focus .field-hint-icon {
    border-color: var(--accent);
    color: var(--accent);
}

/* 弹出层：Teleport 到 body 后用 fixed 定位（坐标由 JS 计算），层级高于弹窗遮罩；
   visibility 与 opacity 一起控制，隐藏时仍保留布局尺寸供测量 */
.field-hint-popup {
    position: fixed;
    left: 0;
    top: 0;
    width: 240px;
    max-height: calc(100vh - 16px);
    overflow-y: auto;
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
    transition: opacity 0.15s ease;
    z-index: 300;
    pointer-events: none;
}

.field-hint-popup.visible {
    visibility: visible;
    opacity: 1;
}
</style>
