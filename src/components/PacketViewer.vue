<script setup lang="ts">
// PacketViewer.vue —— 平台级收发报文查看弹窗（一个平台一份）。
//
// 本弹窗展示某平台下所有 TCP 连接收发的全部报文（平台=业务接入点，
// 报文按平台聚合更符合业务视角），每条报文两行：
//   第一行：时间戳 + 方向（RECV/SEND，颜色区分）+ 十六进制原始字节；
//   第二行：按交易所字段名解析出的字段（与原始报文同屏对照，字体/底色
//           与原始报文区分：等宽小字 vs 无衬线字段块，收发底色分绿/琥珀）。
// 解析失败或未知消息类型时只有第一行（无字段可解析）。
//
// 报文按“连接”分组展示：平台下可能先后接入过多条连接（断开的连接
// 也会保留历史），每条连接一个分组标题（#连接号 + 柜台地址 + 接入时间），
// 组与组之间有明显分界线；连接内按时间顺序排列，跨连接按平台级序号
// （即全局时间顺序）衔接。
//
// 打开弹窗即回看内存里保留的全部历史报文（无论是否持久化到文件）；
// 之后每 0.7 秒向后端拉一次“平台序号大于游标”的新报文追加显示。
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import type { Backend } from "../backend";
import type { PacketConn, PacketPage, PacketRecord } from "../types";

const props = defineProps<{
    /** 后端通道（轮询拉报文用） */
    backend: Backend | null;
    gatewayId: string;
    platformId: string;
    /** 平台名（弹窗标题用） */
    platformName: string;
}>();

const emit = defineEmits<{ (e: "close"): void }>();

/** 已展示的报文（新拉到的一直往后追加） */
const packets = ref<PacketRecord[]>([]);
/** 增量拉取游标：只拉平台序号大于它的新报文 */
const cursor = ref(0);
/** 该平台是否持久化（决定徽标显示“完整历史”还是“内存缓冲”） */
const persist = ref(false);
/** 连接摘要（分组标题信息），key 为连接号 */
const conns = ref<Record<number, PacketConn>>({});
/** 拉取是否正常（平台未运行/未开捕获时轮询失败，停止刷新） */
const alive = ref(true);
/** 报文列表容器（自动滚到底部用） */
const listEl = ref<HTMLElement | null>(null);
/** 是否贴着列表底部：用户在底部时新报文到达自动下滚；
 *  用户滚到历史报文（离开底部）后停留在原处，不抢视线 */
const stickBottom = ref(true);
/** 距底部多少像素内仍算“贴底”（避免亚像素/滚动条宽度引起的抖动） */
const SCROLL_TOLERANCE = 8;

let pollTimer: number | undefined;

/** 拉一批报文并追加展示。showInitial=true 表示弹窗刚打开的那次拉取。
 *  返回 false 表示平台已不可查（后端报错，如未运行/未开捕获） */
async function fetchPackets(afterSeq: number, showInitial: boolean): Promise<boolean> {
    const resp = await props.backend?.dispatch({
        cmd: "get_platform_packets",
        gatewayId: props.gatewayId,
        platformId: props.platformId,
        afterSeq,
    });
    if (!resp || !resp.ok) return false;
    const page = resp.data as PacketPage;
    if (showInitial) {
        persist.value = page.persist;
        // 打开弹窗即回看内存里保留的全部历史（含已断开连接留下的报文）
        packets.value = page.packets;
    } else {
        packets.value.push(...page.packets);
    }
    // 刷新连接摘要（新增连接时分组标题才能显示出来）
    for (const c of page.conns ?? []) {
        conns.value[c.connId] = c;
    }
    cursor.value = page.latestSeq;
    scrollToBottom();
    return true;
}

/** 按连接分组：报文按平台序号（全局时间顺序）排列，连接号变化即开新组；
 *  每组标题展示柜台地址/接入时间/是否仍在线 */
interface ConnGroup {
    connId: number;
    peer: string;
    since: string;
    alive: boolean;
    items: PacketRecord[];
}
const groups = computed<ConnGroup[]>(() => {
    const out: ConnGroup[] = [];
    for (const p of packets.value) {
        const last = out[out.length - 1];
        if (!last || last.connId !== p.connId) {
            const c = conns.value[p.connId];
            out.push({
                connId: p.connId,
                peer: c?.peer ?? "",
                since: c?.since ?? "",
                alive: c?.alive ?? true,
                items: [p],
            });
        } else {
            last.items.push(p);
        }
    }
    return out;
});

/** 新报文追加后把滚动条滑到最底部（像聊天窗口一样）；
 *  仅当滚动条本来就在底部时才自动下滚，用户在看历史报文时保持原位 */
function scrollToBottom() {
    nextTick(() => {
        const el = listEl.value;
        if (el && stickBottom.value) el.scrollTop = el.scrollHeight;
    });
}

/** 滚动监听：实时判断滚动条是否在底部，更新“贴底”状态 */
function onScroll() {
    const el = listEl.value;
    if (!el) return;
    stickBottom.value = el.scrollTop + el.clientHeight >= el.scrollHeight - SCROLL_TOLERANCE;
}

/** 一键清空界面上的报文（仅当次生效）：
 *  游标保留，清空后轮询只追加新到达的报文，不回看历史；
 *  关闭弹窗再打开时仍按原逻辑回看全部历史 */
function clearPackets() {
    packets.value = [];
    stickBottom.value = true;
    scrollToBottom();
}

/** 定时轮询：拉游标之后的新报文 */
async function poll() {
    if (!(await fetchPackets(cursor.value, false))) {
        alive.value = false;
        clearInterval(pollTimer);
    }
}

onMounted(async () => {
    // 打开瞬间先拉一次全量缓冲，回看全部历史
    if (!(await fetchPackets(0, true))) {
        alive.value = false;
        return;
    }
    pollTimer = window.setInterval(poll, 700);
});

// 关闭弹窗时停掉轮询定时器，别让它一直打扰后端
onBeforeUnmount(() => clearInterval(pollTimer));
</script>

<template>
    <!-- @mousedown.self：只有点在遮罩（弹窗外的暗区）才关闭 -->
    <div class="modal-mask" @mousedown.self="emit('close')">
        <div class="modal pv-modal">
            <div class="modal-head">
                <span>收发报文 · {{ platformName }}</span>
                <button class="modal-close" @click="emit('close')">✕</button>
            </div>
            <div class="modal-body">
                <!-- 平台信息 + 展示模式说明 -->
                <div class="pv-meta">
                    <span class="badge" :class="alive ? 'green' : 'gray'">
                        {{ alive ? "平台级聚合" : "平台未运行或未开启报文捕获" }}
                    </span>
                    <span class="badge" :class="persist ? 'blue' : 'gray'">
                        {{ persist ? "持久化 · 完整历史" : "内存缓冲 · 最近 5000 条" }}
                    </span>
                    <button
                        class="btn sm danger-ghost pv-clear"
                        :disabled="!packets.length"
                        title="清空界面上的报文（仅当次生效，关闭后重新打开仍展示全部历史）"
                        @click="clearPackets"
                    >
                        清空
                    </button>
                    <span class="pv-count num">{{ packets.length }} 条 · {{ groups.length }} 个连接</span>
                </div>

                <!-- 报文列表：按连接分组，组间有分界线；行内是 时间戳 + 方向标签 + 十六进制字节 -->
                <div ref="listEl" class="pv-list" @scroll="onScroll">
                    <div v-if="!groups.length" class="pv-empty">
                        {{ alive ? "暂无报文，等待收发…" : "没有可展示的报文" }}
                    </div>
                    <template v-for="g in groups" :key="g.connId">
                        <!-- 连接分组标题：连接号 + 柜台地址 + 接入时间 + 在线状态 -->
                        <div class="pv-conn-head" :class="{ dead: !g.alive }">
                            <span class="dot" :class="g.alive ? 'on' : 'off'"></span>
                            <span class="pv-conn-id num">连接 #{{ g.connId }}</span>
                            <span class="pv-conn-peer num">{{ g.peer }}</span>
                            <span class="pv-conn-since num">{{ g.since }} 接入</span>
                            <span class="pv-conn-state" :class="g.alive ? 'on' : 'off'">
                                {{ g.alive ? "在线" : "已断开" }}
                            </span>
                            <span class="pv-conn-count num">{{ g.items.length }} 条</span>
                        </div>
                        <div v-for="p in g.items" :key="p.seq" class="pv-item" :class="p.dir">
                            <div class="pv-row">
                                <span class="pv-ts num">{{ p.ts }}</span>
                                <span class="pv-dir" :class="p.dir">{{ p.dir === "recv" ? "RECV" : "SEND" }}</span>
                                <span class="pv-hex num">{{ p.hex }}</span>
                            </div>
                            <!-- 解析字段：与原始报文同屏对照展示（字段块底色随收发方向
                                 着色；字体与原始报文区分，一眼分清哪行是字段哪行是字节） -->
                            <div v-if="p.fields && p.fields.length" class="pv-fields" :class="p.dir">
                                <span v-for="(f, i) in p.fields" :key="i" class="pf">
                                    <span class="pf-name">{{ f.name }}</span>
                                    <span class="pf-val">{{ f.value }}</span>
                                </span>
                            </div>
                        </div>
                    </template>
                </div>
            </div>
        </div>
    </div>
</template>

<style scoped>
.pv-modal {
    width: 900px;
    max-width: 94vw;
}

.pv-meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin-bottom: 10px;
}

.pv-count {
    margin-left: auto;
    color: var(--text-faint);
    font-size: 11px;
}

.pv-list {
    height: 60vh;
    max-height: 520px;
    overflow-y: auto;
    background: var(--bg-soft);
    border: 1px solid var(--border-soft);
    border-radius: var(--radius-sm);
    padding: 8px 10px;
    font-family: Consolas, "Courier New", monospace;
    font-size: 12px;
    line-height: 1.7;
}

.pv-empty {
    color: var(--text-faint);
    text-align: center;
    padding: 40px 0;
    font-family: inherit;
}

/* 连接分组标题：把不同连接的报文明显隔开（分界线），
   展示 连接号/柜台地址/接入时间/在线状态/条数 */
.pv-conn-head {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 10px -10px 4px;
    padding: 6px 10px;
    background: rgba(99, 102, 241, 0.1);
    border-top: 1px dashed rgba(99, 102, 241, 0.35);
    border-bottom: 1px solid rgba(99, 102, 241, 0.2);
    font-size: 11.5px;
    font-weight: 600;
    color: var(--text);
}

/* 第一个分组标题不需要顶部虚线（上面没有其他连接） */
.pv-list > .pv-conn-head:first-child {
    margin-top: 0;
    border-top: none;
}

/* 已断开连接的标题：灰色调，一眼区分历史连接 */
.pv-conn-head.dead {
    background: rgba(128, 128, 128, 0.08);
    border-color: rgba(128, 128, 128, 0.25);
    color: var(--text-dim);
}

.pv-conn-id {
    color: var(--accent, #818cf8);
}

.pv-conn-head.dead .pv-conn-id {
    color: var(--text-dim);
}

.pv-conn-peer {
    color: var(--text);
    font-weight: 500;
}

.pv-conn-since {
    color: var(--text-faint);
    font-weight: 400;
}

.pv-conn-state {
    font-size: 10.5px;
    font-weight: 500;
    padding: 0 7px;
    border-radius: 8px;
}

.pv-conn-state.on {
    background: rgba(52, 211, 153, 0.15);
    color: var(--green);
}

.pv-conn-state.off {
    background: rgba(128, 128, 128, 0.15);
    color: var(--text-faint);
}

.pv-conn-count {
    margin-left: auto;
    color: var(--text-faint);
    font-size: 10.5px;
    font-weight: 400;
}

.pv-row {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: 12px;
    padding: 2px 4px;
    border-radius: 4px;
    white-space: normal;
}

/* 一条报文的整体容器：原始报文行 + 解析字段行。
   收/发方向决定字段行的底色（绿 = 收到，琥珀 = 发送） */
.pv-item {
    border-radius: 6px;
    padding: 3px 4px;
    margin: 1px 0;
}

.pv-item:hover {
    background: rgba(255, 255, 255, 0.04);
}

/* 原始报文行：等宽小字，保持“数据”观感；
   与下方的解析字段（无衬线字体）形成字体差异 */
.pv-item .pv-row {
    padding: 0;
    font-size: 11.5px;
}
/* 解析字段行：无衬线小字，字段块可换行排列；
   底色按收发方向区分（与方向标签同色系但更淡，不抢正文） */
.pv-fields {
    display: flex;
    flex-wrap: wrap;
    gap: 3px 6px;
    padding: 4px 6px;
    margin: 2px 0 2px 56px;
    border-radius: 6px;
    font-family: inherit;
    font-size: 11px;
    line-height: 1.5;
}

/* 收到（recv）：绿色调底色 */
.pv-fields.recv {
    background: rgba(52, 211, 153, 0.07);
    border-left: 2px solid rgba(52, 211, 153, 0.35);
}

/* 发送（send）：琥珀色调底色 */
.pv-fields.send {
    background: rgba(251, 191, 36, 0.07);
    border-left: 2px solid rgba(251, 191, 36, 0.35);
}
/* 单个字段块：字段名统一着色（紫蓝），值保持正文色 */
.pf {
    display: inline-flex;
    gap: 4px;
    white-space: nowrap;
    max-width: 100%;
}

.pf-name {
    color: var(--accent, #818cf8);
    font-weight: 600;
}

.pf-val {
    color: var(--text);
}

.pv-item.recv .pf-name {
    color: var(--green);
}

.pv-item.send .pf-name {
    color: var(--amber);
}

/* 字段值颜色跟随收发方向（与 SEND/RECV 标签同色） */
.pv-item.recv .pf-val {
    color: var(--green);
}

.pv-item.send .pf-val {
    color: var(--amber);
}

/* 时间戳：淡灰小字 */
.pv-ts {
    color: var(--text-faint);
    flex-shrink: 0;
}

/* 方向标签：RECV 绿色（收），SEND 琥珀色（发） */
.pv-dir {
    flex-shrink: 0;
    width: 44px;
    font-weight: 700;
}

.pv-dir.recv {
    color: var(--green);
}

.pv-dir.send {
    color: var(--amber);
}

/* 十六进制正文：原始字节长时像解析字段一样换行全部展示（不截断），
   字体比解析字段再小一号并改斜体，一眼分清哪行是字节；
   颜色跟随收发方向（与 SEND/RECV 标签同色） */
.pv-hex {
    min-width: 0;
    flex: 1;
    font-size: 10.5px;
    font-style: italic;
    word-break: break-all;
    white-space: normal;
}

.pv-item.recv .pv-hex {
    color: var(--green);
}

.pv-item.send .pv-hex {
    color: var(--amber);
}
</style>
