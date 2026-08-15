<script setup lang="ts">
// LogPanel.vue —— 窗口底部的运行日志面板。
// 日志数据本身存在父组件 App.vue 里（props 传入），
// 本组件只负责展示：折叠/展开、按级别过滤、自动滚到最新一条。
import { computed, nextTick, ref, watch } from "vue";
import type { LogEvent } from "../types";

const props = defineProps<{
    /** 全部日志（父组件维护，最多 800 条） */
    logs: LogEvent[];
}>();

// 点“清空”时通知父组件把数组清掉（数据在谁手里就由谁改）
const emit = defineEmits<{
    (e: "clear"): void;
}>();

/** 面板是否收起（只剩标题栏一条） */
const collapsed = ref(false);
/** 当前级别过滤器：全部 / 仅 info / 仅 warn / 仅 error */
const filter = ref<"all" | "info" | "warn" | "error">("all");
/** 新日志到来时是否自动滚到底部（想回看旧日志时可取消勾选） */
const autoScroll = ref(true);
/** 日志列表的 DOM 引用（模板里 ref="listEl"），滚动操作需要直接碰它 */
const listEl = ref<HTMLElement | null>(null);

/** 过滤后实际显示的日志 */
const shown = computed(() =>
    filter.value === "all" ? props.logs : props.logs.filter((l) => l.level === filter.value),
);

// watch：监听日志条数变化，有新日志就滚到底部。
// nextTick 的作用：等 Vue 把新日志行真正画到页面上之后再滚，
// 否则滚动时新行还不存在，会差一行滚不到底。
watch(
    () => props.logs.length,
    async () => {
        if (autoScroll.value && !collapsed.value) {
            await nextTick();
            listEl.value?.scrollTo({ top: listEl.value.scrollHeight });
        }
    },
);
</script>

<template>
    <div class="logpanel" :class="{ collapsed }">
        <!-- 标题栏：点标题折叠/展开；右侧是过滤按钮组、自动滚动开关、清空按钮 -->
        <div class="log-head">
            <div class="log-title" @click="collapsed = !collapsed">
                <span class="chevron">{{ collapsed ? "▸" : "▾" }}</span>
                运行日志
                <span class="badge gray num">{{ logs.length }}</span>
            </div>
            <div class="log-tools" v-if="!collapsed">
                <div class="filter-group">
                    <button
                        v-for="f in ['all', 'info', 'warn', 'error'] as const"
                        :key="f"
                        class="filter-btn"
                        :class="{ active: filter === f }"
                        @click="filter = f"
                    >
                        {{ f === "all" ? "全部" : f }}
                    </button>
                </div>
                <label class="field-row" style="gap: 4px">
                    <input v-model="autoScroll" type="checkbox" />
                    自动滚动
                </label>
                <button class="btn sm ghost" @click="emit('clear')">清空</button>
            </div>
        </div>
        <!-- 日志列表：每行 = 时间 + 级别 + 消息，颜色随级别变化 -->
        <div v-if="!collapsed" ref="listEl" class="log-list">
            <div v-if="!shown.length" class="log-empty">暂无日志</div>
            <div v-for="(l, i) in shown" :key="i" class="log-row" :class="l.level">
                <span class="log-ts num">{{ l.ts }}</span>
                <span class="log-level" :class="l.level">{{ l.level.toUpperCase() }}</span>
                <span class="log-msg">{{ l.message }}</span>
            </div>
        </div>
    </div>
</template>

<style scoped>
.logpanel {
    border-top: 1px solid var(--border);
    background: var(--bg-soft);
    display: flex;
    flex-direction: column;
    height: 200px;
    flex-shrink: 0;
}

.logpanel.collapsed {
    height: auto;
}

.log-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 7px 16px;
}

.log-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12.5px;
    font-weight: 600;
    cursor: pointer;
    user-select: none;
}

.chevron {
    color: var(--text-faint);
    font-size: 11px;
}

.log-tools {
    display: flex;
    align-items: center;
    gap: 14px;
    font-size: 12px;
    color: var(--text-dim);
}

.filter-group {
    display: flex;
    background: var(--panel);
    border: 1px solid var(--border-soft);
    border-radius: var(--radius-sm);
    overflow: hidden;
}

.filter-btn {
    border: none;
    background: transparent;
    color: var(--text-dim);
    padding: 3px 10px;
    font-size: 11.5px;
    cursor: pointer;
}

.filter-btn.active {
    background: var(--accent-soft);
    color: var(--accent);
}

.log-list {
    flex: 1;
    overflow-y: auto;
    padding: 2px 16px 10px;
    font-family: var(--mono);
    font-size: 11.5px;
}

.log-empty {
    color: var(--text-faint);
    padding: 12px 0;
    text-align: center;
    font-family: inherit;
}

.log-row {
    display: flex;
    gap: 10px;
    padding: 2px 0;
    line-height: 1.55;
}

.log-ts {
    color: var(--text-faint);
    flex-shrink: 0;
}

.log-level {
    flex-shrink: 0;
    width: 42px;
    font-weight: 600;
}

.log-level.info {
    color: var(--accent);
}

.log-level.warn {
    color: var(--amber);
}

.log-level.error {
    color: var(--red);
}

.log-msg {
    color: var(--text);
    word-break: break-all;
    white-space: pre-wrap;
}

.log-row.warn .log-msg {
    color: var(--amber);
}

.log-row.error .log-msg {
    color: var(--red);
}
</style>
