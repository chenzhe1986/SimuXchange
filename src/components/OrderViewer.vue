<script setup lang="ts">
// OrderViewer.vue —— 平台订单列表弹窗（点平台卡片的“订单”按钮打开）。
//
// 展示平台开启“缓存订单”后收到的全部委托及其实时状态：
// 每行一笔订单，含委托编号/交易所订单号/证券/方向/价格/数量/成交/剩余/状态。
// 右上角勾选“只显示在途订单”后，列表只保留已报/部分成交的订单
// （在途 = 撤单会成功；全成/已拒/已撤是终态，撤单会失败）。
//
// 在途订单每行有“回复”按钮：点击展开回复面板，可手动选择回复
// 成交回报（数量/价格可配）、拒单回报（原因代码可配）或撤单成功回报，
// 面板里的输入默认按订单缓存兜底（剩余量/委托价/原因 1），发送后订单进入终态。
//
// 实现方式：打开时拉一次全量，之后每 1 秒轮询一次刷新（订单状态随回报实时变化）。
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import type { Backend } from "../backend";
import type { GatewayCategory, OrderEntry } from "../types";
import { isInflight, orderStatusOf, defaultRejectReason } from "../types";
import { rejectTextOf } from "../errors";

const props = defineProps<{
    /** 后端通道（轮询拉订单用） */
    backend: Backend | null;
    gatewayId: string;
    platformId: string;
    /** 平台名（弹窗标题用） */
    platformName: string;
    /** 平台监听地址（副标题用） */
    listenAddr: string;
    /** 所属网关分类（拒单原因码默认值与错误码表按分类选取） */
    category: GatewayCategory;
}>();

const emit = defineEmits<{
    (e: "close"): void;
    /** 手动回复成功/失败的消息（交给父组件弹 toast） */
    (e: "notify", text: string, isError?: boolean): void;
}>();

/** 全部缓存订单（后端返回时最新在前） */
const orders = ref<OrderEntry[]>([]);
/** 是否只显示在途订单（已报/部分成交） */
const onlyInflight = ref(false);
/** 加载失败/连接断开标记（停止轮询并提示） */
const failed = ref(false);

let pollTimer: number | undefined;

/** 拉一次全量订单并整体替换 */
async function fetchOrders(): Promise<boolean> {
    const resp = await props.backend?.dispatch({
        cmd: "get_orders",
        gatewayId: props.gatewayId,
        platformId: props.platformId,
    });
    if (!resp || !resp.ok) return false;
    orders.value = (resp.data as OrderEntry[]) ?? [];
    return true;
}

/** 勾选“只显示在途订单”时过滤；未勾选展示全部 */
const shown = computed(() =>
    onlyInflight.value ? orders.value.filter((o) => isInflight(o.status)) : orders.value,
);

onMounted(async () => {
    if (!(await fetchOrders())) {
        failed.value = true;
        return;
    }
    pollTimer = window.setInterval(async () => {
        if (!(await fetchOrders())) {
            failed.value = true;
            clearInterval(pollTimer);
        }
    }, 1000);
});

// 关闭弹窗时停掉轮询定时器
onBeforeUnmount(() => clearInterval(pollTimer));

/** 数量格式化：整数不带小数位，非整数保留两位（债券按张可能是小数） */
function fmtQty(n: number): string {
    return Number.isInteger(n) ? String(n) : n.toFixed(2);
}

/** 价格格式化：最多 5 位小数（协议精度），去掉多余的 0 */
function fmtPrice(n: number): string {
    return String(Number(n.toFixed(5)));
}

// ---- 手动回复（在途单） ----

/** 当前展开回复面板的订单（null = 没展开） */
const expanded = ref<OrderEntry | null>(null);
/** 回复面板的编辑值：成交数量（默认剩余量）/ 成交价格（默认委托价）/ 拒单原因代码 */
const replyQty = ref(0);
const replyPrice = ref(0);
const replyReason = ref(defaultRejectReason(props.category));
/** 拒单原因码在错误码表里的说明（输入框下方的提示文字） */
const replyReasonText = computed(() => rejectTextOf(props.category, replyReason.value));
/** 发送中标记（防止连点重复发送） */
const sending = ref(false);

/** 点击“回复”：展开/收起该订单的回复面板，展开时预填默认值 */
function expandRow(o: OrderEntry) {
    if (expanded.value?.clOrdId === o.clOrdId) {
        expanded.value = null;
        return;
    }
    expanded.value = o;
    replyQty.value = o.leavesQty;
    replyPrice.value = o.price;
    // 拒单原因码默认按网关分类：深圳 20009 / 上海 1025
    replyReason.value = defaultRejectReason(props.category);
}

/** 发送手动回报：kind 对应后端 ManualReportKind（trade/reject/cancel） */
async function sendReply(kind: "trade" | "reject" | "cancel") {
    const o = expanded.value;
    if (!o || sending.value) return;
    if (kind === "trade" && replyQty.value <= 0) {
        emit("notify", "成交数量必须大于 0", true);
        return;
    }
    if (kind === "trade" && replyPrice.value <= 0) {
        emit("notify", "成交价格必须大于 0", true);
        return;
    }
    sending.value = true;
    const payload: Record<string, unknown> = {
        cmd: "send_report",
        gatewayId: props.gatewayId,
        platformId: props.platformId,
        clOrdId: o.clOrdId,
        kind,
    };
    if (kind === "trade") {
        payload.qty = replyQty.value;
        payload.price = replyPrice.value;
    } else if (kind === "reject") {
        payload.reason = replyReason.value;
    }
    const resp = await props.backend?.dispatch(payload);
    sending.value = false;
    if (!resp || !resp.ok) {
        emit("notify", resp?.error ?? "发送失败", true);
        return;
    }
    const desc = (resp.data as { desc?: string } | null)?.desc ?? "回报已发送";
    emit("notify", desc);
    expanded.value = null; // 订单已进入终态，收起面板
    await fetchOrders(); // 状态变了，立即刷新列表
}
</script>

<template>
    <!-- @mousedown.self：只有点在遮罩（弹窗外的暗区）才关闭 -->
    <div class="modal-mask" @mousedown.self="emit('close')">
        <div class="modal ov-modal">
            <div class="modal-head">
                <span>订单 · {{ platformName }}</span>
                <button class="modal-close" @click="emit('close')">✕</button>
            </div>
            <div class="modal-body">
                <!-- 平台信息 + 在途过滤勾选 -->
                <div class="ov-meta">
                    <span class="badge gray num">{{ listenAddr }}</span>
                    <span class="badge blue">共 {{ orders.length }} 笔</span>
                    <label class="ov-filter">
                        <input v-model="onlyInflight" type="checkbox" />
                        只显示在途订单
                    </label>
                </div>

                <!-- 订单表格 -->
                <div class="ov-table-wrap">
                    <table class="ov-table">
                        <thead>
                            <tr>
                                <th>时间</th>
                                <th>连接</th>
                                <th>委托编号</th>
                                <th>交易所订单号</th>
                                <th>证券</th>
                                <th>方向</th>
                                <th class="num">价格</th>
                                <th class="num">委托量</th>
                                <th class="num">成交量</th>
                                <th class="num">剩余</th>
                                <th>状态</th>
                                <th>操作</th>
                            </tr>
                        </thead>
                        <tbody>
                            <tr v-if="failed">
                                <td colspan="12" class="ov-empty">订单查询失败，请检查后端连接后重试</td>
                            </tr>
                            <tr v-else-if="!shown.length">
                                <td colspan="12" class="ov-empty">
                                    {{ onlyInflight ? "没有在途订单" : "暂无订单，等待柜台委托…" }}
                                </td>
                            </tr>
                            <template v-for="o in shown" :key="o.clOrdId">
                                <tr>
                                    <td class="num ts">{{ o.ts }}</td>
                                    <td>
                                        <!-- 订单来自哪条连接（历史连接发来的订单也保留展示） -->
                                        <span class="ov-conn num">#{{ o.connId }}</span>
                                    </td>
                                    <td class="num">{{ o.clOrdId }}</td>
                                    <td class="num oid">{{ o.orderId || "—" }}</td>
                                    <td class="num">{{ o.securityId }}</td>
                                    <td :class="o.side === '买' ? 'side-buy' : 'side-sell'">{{ o.side }}</td>
                                    <td class="num">{{ fmtPrice(o.price) }}</td>
                                    <td class="num">{{ fmtQty(o.qty) }}</td>
                                    <td class="num">{{ fmtQty(o.cumQty) }}</td>
                                    <td class="num">{{ fmtQty(o.leavesQty) }}</td>
                                    <td>
                                        <span class="badge" :class="orderStatusOf(o.status).cls">
                                            {{ orderStatusOf(o.status).label }}
                                        </span>
                                    </td>
                                    <td>
                                        <!-- 只有“在途”订单能手动回复（已报/部分成交） -->
                                        <button v-if="isInflight(o.status)" class="btn sm ghost reply-btn" @click="expandRow(o)">
                                            回复
                                        </button>
                                        <span v-else class="reply-none">—</span>
                                    </td>
                                </tr>
                                <!-- 手动回复面板：展开在订单行下方，三种回报各占一块 -->
                                <tr v-if="expanded?.clOrdId === o.clOrdId" class="ov-reply-row">
                                    <td colspan="12">
                                        <div class="ov-reply">
                                            <div class="ov-reply-title">
                                                手动回复 · 委托 <span class="num">{{ o.clOrdId }}</span>
                                                <span class="ov-reply-hint">发送后订单进入终态，不能再回复</span>
                                            </div>
                                            <div class="ov-reply-body">
                                                <!-- 成交回报：数量/价格可配置，默认剩余量全成 + 委托价 -->
                                                <div class="ov-reply-block">
                                                    <div class="ov-reply-label">成交回报</div>
                                                    <div class="ov-reply-fields">
                                                        <label>数量 <input v-model.number="replyQty" type="number" min="1" step="1" /></label>
                                                        <label>价格 <input v-model.number="replyPrice" type="number" min="0.0001" step="0.0001" /></label>
                                                    </div>
                                                    <button class="btn sm success" :disabled="sending" @click="sendReply('trade')">
                                                        发送成交
                                                    </button>
                                                </div>
                                                <!-- 拒单回报：拒单原因代码可配置，默认按网关分类（深圳 20009 / 上海 1025），
                                                     下方提示错误码表里的对应说明 -->
                                                <div class="ov-reply-block">
                                                    <div class="ov-reply-label">拒单回报</div>
                                                    <div class="ov-reply-fields">
                                                        <label>原因代码 <input v-model.number="replyReason" type="number" min="0" step="1" /></label>
                                                        <span class="ov-reply-note" v-if="replyReasonText">{{ replyReasonText }}</span>
                                                    </div>
                                                    <button class="btn sm danger-ghost" :disabled="sending" @click="sendReply('reject')">
                                                        发送拒单
                                                    </button>
                                                </div>
                                                <!-- 撤单成功回报：无需参数，剩余量全部撤掉 -->
                                                <div class="ov-reply-block">
                                                    <div class="ov-reply-label">撤单成功回报</div>
                                                    <div class="ov-reply-note">剩余 {{ fmtQty(o.leavesQty) }} 全部撤掉</div>
                                                    <button class="btn sm ghost" :disabled="sending" @click="sendReply('cancel')">
                                                        发送撤成
                                                    </button>
                                                </div>
                                            </div>
                                            <!-- 回报发送去向：平台同一时刻只接一个柜台，回报统一从当前连接发出 -->
                                            <div class="ov-reply-note ov-reply-target">
                                                回报将发往当前活动连接（历史连接发来的订单同样走当前连接）
                                            </div>
                                        </div>
                                    </td>
                                </tr>
                            </template>
                        </tbody>
                    </table>
                </div>
            </div>
        </div>
    </div>
</template>

<style scoped>
.ov-modal {
    width: 900px;
    max-width: 94vw;
}

.ov-meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin-bottom: 10px;
}

/* 在途过滤勾选：靠右显示，点击整行可切换 */
.ov-filter {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--text-dim);
    cursor: pointer;
    user-select: none;
}

.ov-table-wrap {
    max-height: 60vh;
    overflow-y: auto;
    border: 1px solid var(--border-soft);
    border-radius: var(--radius-sm);
}

.ov-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
}

.ov-table th {
    position: sticky;
    top: 0;
    background: var(--bg-soft);
    color: var(--text-faint);
    font-weight: 500;
    text-align: left;
    padding: 7px 10px;
    border-bottom: 1px solid var(--border-soft);
    white-space: nowrap;
    z-index: 1;
}

.ov-table td {
    padding: 6px 10px;
    border-bottom: 1px solid var(--border-soft);
    white-space: nowrap;
}

.ov-table tbody tr:last-child td {
    border-bottom: none;
}

.ov-table tbody tr:hover {
    background: rgba(255, 255, 255, 0.04);
}

/* 数字列右对齐（价格/数量），订单号等编号左对齐即可 */
.ov-table .num {
    font-family: Consolas, "Courier New", monospace;
}

.ov-table td.num {
    text-align: right;
}

.ov-table th.num {
    text-align: right;
}

.ts {
    color: var(--text-faint);
}

.oid {
    color: var(--text-dim);
}

.side-buy {
    color: var(--red);
    font-weight: 600;
}

.side-sell {
    color: var(--green);
    font-weight: 600;
}

.ov-empty {
    text-align: center;
    color: var(--text-faint);
    padding: 32px 0 !important;
}

/* 连接号小徽标（订单来自哪条连接） */
.ov-conn {
    display: inline-block;
    min-width: 26px;
    padding: 0 6px;
    border-radius: 8px;
    background: rgba(99, 102, 241, 0.15);
    color: var(--accent, #818cf8);
    font-size: 10.5px;
    font-weight: 600;
    text-align: center;
}

/* 回复面板底部的发送去向提示 */
.ov-reply-target {
    margin-top: 10px;
    padding-top: 8px;
    border-top: 1px dashed var(--border);
}

/* ---- 手动回复：操作列按钮 + 展开面板 ---- */

.reply-btn {
    padding: 1px 10px;
    font-size: 11px;
}

.reply-none {
    color: var(--text-faint);
}

.ov-reply-row td {
    background: var(--bg-soft);
    padding: 12px 14px;
}

.ov-reply-title {
    display: flex;
    align-items: baseline;
    gap: 8px;
    font-size: 12px;
    color: var(--text-dim);
    margin-bottom: 10px;
}

.ov-reply-hint {
    color: var(--text-faint);
    font-size: 10.5px;
}

.ov-reply-body {
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
}

/* 三种回报各占一块，输入框与按钮纵向排列 */
.ov-reply-block {
    flex: 1;
    min-width: 200px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px 12px;
    background: var(--panel);
    border: 1px solid var(--border-soft);
    border-radius: var(--radius-sm);
}

.ov-reply-label {
    font-size: 11.5px;
    font-weight: 600;
    color: var(--text);
}

.ov-reply-fields {
    display: flex;
    flex-direction: column;
    gap: 6px;
}

.ov-reply-fields label {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    font-size: 11px;
    color: var(--text-dim);
}

.ov-reply-fields input {
    width: 96px;
    padding: 3px 6px;
    font-size: 11.5px;
}

.ov-reply-note {
    font-size: 11px;
    color: var(--text-faint);
    line-height: 1.5;
}
</style>
