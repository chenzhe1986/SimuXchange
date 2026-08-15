<script setup lang="ts">
// PlatformCard.vue —— 主区网格里的“平台卡片”，一个平台一张。
// 展示平台的配置摘要（类型/地址/策略）、实时统计和当前连接列表。
//
// 子组件通信约定：
//   props（父 → 子）：数据单向流入，子组件只读不改
//   emit （子 → 父）：子组件仅上报事件（如 "edit"），处理逻辑由父组件
//                     决定——职责分离，组件可复用
import { computed, ref } from "vue";
import type { GatewayCategory, PlatformConfig, PlatformSnapshot } from "../types";
import { delayLabel, emptyStats, platformTypeLabel, strategyModeLabel } from "../types";

// 父组件传入的数据
const props = defineProps<{
    /** 平台的静态配置（名字/端口/策略…） */
    platform: PlatformConfig;
    /** 所属网关分类（决定平台类型标签用哪套对照表） */
    category: GatewayCategory;
    /** 平台的运行状态快照（网关未启动时可能为空） */
    snapshot?: PlatformSnapshot;
    /** 所属网关是否运行中（运行中禁止删除） */
    gatewayRunning: boolean;
}>();

// 向父组件发出的事件：点了“编辑”/“删除”按钮 / 查看平台报文 / 查看订单缓存
const emit = defineEmits<{
    (e: "edit"): void;
    (e: "remove"): void;
    (e: "viewPackets"): void;
    (e: "viewOrders"): void;
}>();

/** 连接列表是否展开（点卡片底部那行切换） */
const showConns = ref(false);

// 下面几个 computed 把“快照可能为空”的情况兜住，
// 给模板提供永远可用的默认值（空统计/空列表/未监听）
const stats = computed(() => props.snapshot?.stats ?? emptyStats());
const conns = computed(() => props.snapshot?.connections ?? []);
const listening = computed(() => props.snapshot?.listening ?? false);
/** 已完成 Logon 登录的连接数（TCP 连上但还没登录的不算） */
const loggedOnCount = computed(() => conns.value.filter((c) => c.loggedOn).length);
/** 平台是否开启收发报文展示（决定是否显示“报文”按钮） */
const showPackets = computed(() => props.platform.showPackets);
/** 平台是否开启订单缓存（决定是否显示“订单”按钮） */
const cacheOrders = computed(() => props.platform.cacheOrders);

/** 五个统计格子的数据（拒单 = 执行报告拒绝 + 业务拒绝之和），
 *  拼成数组后模板里用 v-for 一次性生成，不用写五段重复 HTML */
const statItems = computed(() => [
    { label: "委托", value: stats.value.orders, cls: "" },
    { label: "确认", value: stats.value.acks, cls: "c-blue" },
    { label: "成交", value: stats.value.trades, cls: "c-green" },
    { label: "拒单", value: stats.value.orderRejects + stats.value.businessRejects, cls: "c-red" },
    { label: "撤单", value: stats.value.cancels, cls: "c-amber" },
]);
</script>

<template>
    <!-- listening 时加 live 类：边框变绿，一眼看出哪些平台在监听 -->
    <div class="pcard" :class="{ live: listening }">
        <!-- 头部：状态圆点 + 平台名 + 悬停才显现的编辑/删除按钮 -->
        <div class="pcard-head">
            <div class="pcard-title">
                <span class="dot" :class="listening ? 'on pulse' : 'off'"></span>
                <span class="name">{{ platform.name }}</span>
            </div>
            <div class="pcard-actions">
                <button class="btn sm ghost" @click="emit('edit')">编辑</button>
                <button class="btn sm ghost danger-ghost" :disabled="gatewayRunning" @click="emit('remove')">删除</button>
            </div>
        </div>

        <!-- 配置摘要：平台类型 / 监听地址 / 监听状态 -->
        <div class="pcard-meta">
            <span class="badge blue">{{ platformTypeLabel(platform.platformType, category) }}</span>
            <span class="badge gray num">{{ platform.listenHost }}:{{ platform.port }}</span>
            <span class="badge" :class="listening ? 'green' : 'gray'">{{ listening ? "监听中" : "未启动" }}</span>
        </div>

        <!-- 策略摘要：回报模式 + 确认/成交延迟 -->
        <div class="pcard-strategy">
            <span class="strategy-mode">{{ strategyModeLabel(platform.strategy.mode) }}</span>
            <span class="strategy-delay">
                确认 {{ delayLabel(platform.strategy.ackDelay) }} · 成交 {{ delayLabel(platform.strategy.tradeDelay) }}
            </span>
        </div>

        <!-- 五格实时统计 -->
        <div class="pcard-stats">
            <div v-for="s in statItems" :key="s.label" class="stat">
                <div class="stat-value num" :class="s.cls">{{ s.value }}</div>
                <div class="stat-label">{{ s.label }}</div>
            </div>
        </div>

        <!-- 连接概览（点击展开/收起下方明细列表） -->
        <div class="pcard-conns" @click="showConns = !showConns">
            <span class="conn-summary">
                <span class="dot" :class="conns.length ? 'on' : 'off'"></span>
                连接 {{ conns.length }}（已登录 {{ loggedOnCount }}）· 累计 {{ stats.totalConnections }}
            </span>
            <span class="conn-side">
                <!-- 订单按钮：开启缓存订单时显示，点击弹窗查看全部缓存订单 -->
                <button v-if="cacheOrders" class="btn sm ghost orders-btn" @click.stop="emit('viewOrders')">
                    订单
                </button>
                <!-- 报文按钮：开启收发报文展示时显示，点击弹窗查看平台全部连接的报文 -->
                <button v-if="showPackets" class="btn sm ghost orders-btn" @click.stop="emit('viewPackets')">
                    报文
                </button>
                <span class="conn-toggle">{{ showConns ? "收起 ▴" : "展开 ▾" }}</span>
            </span>
        </div>
        <!-- 连接明细：每行是一个已连接的柜台（对方地址/机构代码/接入时间），纯展示 -->
        <transition name="fade">
            <div v-if="showConns && conns.length" class="conn-list">
                <div v-for="c in conns" :key="c.id" class="conn-row">
                    <span class="dot" :class="c.loggedOn ? 'on' : 'off'"></span>
                    <span class="num peer">{{ c.peer }}</span>
                    <span class="comp">{{ c.compId || "未登录" }}</span>
                    <span class="since num">{{ c.since }}</span>
                </div>
            </div>
        </transition>
    </div>
</template>

<style scoped>
.pcard {
    background: var(--panel);
    border: 1px solid var(--border-soft);
    border-radius: var(--radius);
    padding: 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    transition: border-color 0.2s;
}

.pcard.live {
    border-color: rgba(52, 211, 153, 0.35);
}

.pcard-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
}

.pcard-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 14px;
    font-weight: 600;
}

.pcard-actions {
    display: flex;
    gap: 6px;
    opacity: 0;
    transition: opacity 0.15s;
}

.pcard:hover .pcard-actions {
    opacity: 1;
}

.pcard-meta {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
}

.pcard-strategy {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    padding: 8px 10px;
    background: var(--bg-soft);
    border-radius: var(--radius-sm);
    font-size: 12px;
}

.strategy-mode {
    color: var(--text);
    font-weight: 500;
}

.strategy-delay {
    color: var(--text-faint);
    font-size: 11px;
}

.pcard-stats {
    display: grid;
    grid-template-columns: repeat(5, 1fr);
    gap: 6px;
}

.stat {
    text-align: center;
    padding: 8px 2px;
    background: var(--bg-soft);
    border-radius: var(--radius-sm);
}

.stat-value {
    font-size: 17px;
    font-weight: 600;
}

.stat-label {
    font-size: 11px;
    color: var(--text-faint);
    margin-top: 2px;
}

.c-blue {
    color: var(--accent);
}

.c-green {
    color: var(--green);
}

.c-red {
    color: var(--red);
}

.c-amber {
    color: var(--amber);
}

.pcard-conns {
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-size: 12px;
    color: var(--text-dim);
    cursor: pointer;
    user-select: none;
}

.conn-summary {
    display: inline-flex;
    align-items: center;
    gap: 7px;
}

.conn-toggle {
    font-size: 11px;
    color: var(--text-faint);
}

.conn-side {
    display: inline-flex;
    align-items: center;
    gap: 8px;
}

.orders-btn {
    padding: 1px 10px;
    font-size: 11px;
}

.conn-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-height: 130px;
    overflow-y: auto;
}

.conn-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 9px;
    background: var(--bg-soft);
    border-radius: var(--radius-sm);
    font-size: 11.5px;
}

.conn-row .peer {
    color: var(--text);
}

.conn-row .comp {
    color: var(--text-dim);
    flex: 1;
}

.conn-row .since {
    color: var(--text-faint);
    font-size: 10.5px;
}

.fade-enter-active,
.fade-leave-active {
    transition: opacity 0.15s;
}

.fade-enter-from,
.fade-leave-to {
    opacity: 0;
}
</style>
