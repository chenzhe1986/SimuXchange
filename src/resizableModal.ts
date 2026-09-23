// resizableModal.ts —— 查看类弹窗“拖动调整大小 + 最大化/还原”的共享逻辑。
//
// OrderViewer / PacketViewer 两个弹窗共用：返回窗口宽高（px）与拖拽/最大化
// 处理函数，模板里把 style 绑到 .modal 上即可。尺寸用 px 而非百分比，
// 拖拽时逐像素跟随鼠标，不会因百分比取整而抖跳。
import { computed, onBeforeUnmount, ref } from "vue";
import type { CSSProperties } from "vue";

export interface ResizableOpts {
    /** 默认窗口宽（px，超出屏幕时自动收窄到视口减边距） */
    defaultW: number;
    /** 默认窗口高（px） */
    defaultH: number;
    /** 最小窗口宽（px）：再小内容布局会被挤压（订单表的列/回报块会被裁） */
    minW: number;
    /** 最小窗口高（px） */
    minH: number;
}

/** 视口四周留的边距（px）：拖拽上限/最大化都按“视口 - 边距”计算 */
const MARGIN = 24;

export function useResizableModal(opts: ResizableOpts) {
    const width = ref(Math.min(opts.defaultW, window.innerWidth - MARGIN));
    const height = ref(Math.min(opts.defaultH, window.innerHeight - MARGIN));
    /** 是否处于最大化（最大化时锁住拖拽，再点按钮还原） */
    const maximized = ref(false);

    /** 进入最大化前记住的尺寸，还原时回到它而不是回默认尺寸 */
    let savedW = width.value;
    let savedH = height.value;
    let dragging = false;
    let startX = 0;
    let startY = 0;
    let baseW = 0;
    let baseH = 0;

    function onMove(e: PointerEvent) {
        if (!dragging) return;
        const maxW = window.innerWidth - MARGIN;
        const maxH = window.innerHeight - MARGIN;
        width.value = Math.min(maxW, Math.max(opts.minW, baseW + (e.clientX - startX)));
        height.value = Math.min(maxH, Math.max(opts.minH, baseH + (e.clientY - startY)));
    }

    function onUp() {
        if (!dragging) return;
        dragging = false;
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", onUp);
    }

    /** 右下角拖拽把手按下：开始跟踪鼠标，宽高钳制在 min/max 之间 */
    function startResize(e: PointerEvent) {
        if (maximized.value) return; // 最大化状态不允许拖拽
        e.preventDefault();
        dragging = true;
        startX = e.clientX;
        startY = e.clientY;
        baseW = width.value;
        baseH = height.value;
        document.body.style.cursor = "nwse-resize";
        document.body.style.userSelect = "none"; // 拖拽中禁选文字，避免拖动时选中内容
        window.addEventListener("pointermove", onMove);
        window.addEventListener("pointerup", onUp);
    }

    /** 最大化/还原切换：最大化前记住当前尺寸，还原时恢复它 */
    function toggleMaximize() {
        if (maximized.value) {
            maximized.value = false;
            width.value = Math.min(savedW, window.innerWidth - MARGIN);
            height.value = Math.min(savedH, window.innerHeight - MARGIN);
        } else {
            savedW = width.value;
            savedH = height.value;
            maximized.value = true;
            width.value = window.innerWidth - MARGIN;
            height.value = window.innerHeight - MARGIN;
        }
    }

    // 组件卸载（弹窗关闭）时清理全局监听，防止拖到一半关窗后监听器泄露
    onBeforeUnmount(() => {
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", onUp);
    });

    /** 绑定到 .modal 的 style：显式宽高 + 最小/最大钳制
     *  （覆盖全局 .modal 的 max-width: 92vw 等，最大化才能撑满视口） */
    const style = computed<CSSProperties>(() => ({
        width: `${width.value}px`,
        height: `${height.value}px`,
        minWidth: `${opts.minW}px`,
        minHeight: `${opts.minH}px`,
        maxWidth: `calc(100vw - ${MARGIN}px)`,
        maxHeight: `calc(100vh - ${MARGIN}px)`,
    }));

    return { width, height, maximized, style, startResize, toggleMaximize };
}