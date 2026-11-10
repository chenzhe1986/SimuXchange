<script setup lang="ts">
// PositionSyncDialog.vue —— “同步持仓数据”弹窗（平台编辑器的 ETF 清单路径下方按钮触发）。
//
// 分两段：
// 1. Oracle 连接信息（环境名/连接串/用户名/密码）与资金账号；
// 2. 点击“查询持仓”后显示该账号的全部持仓，六列均可编辑。
//
// 数据流：
// - 打开时先拉服务端快照（get_position_state）：回填上次使用的连接信息/
//   资金账号，并把内存中当前生效的持仓显示出来（没有则空表）；
// - “查询持仓”只把结果拉到界面（query_positions），不写入服务端内存；
// - “保存”才整份替换服务端内存中的持仓（save_positions），然后关闭弹窗。
// 持仓是全局唯一一份，保存后下一次 ETF 申购立即按新数据扣减。
//
// 查询在服务端 blocking 线程池执行（见 positions.rs），界面显示“查询中”
// 期间不影响网关收发与其他弹窗。
import { onMounted, reactive, ref } from "vue";
import type { Backend } from "../backend";
import type { OracleConnectConfig, PositionRow, PositionSnapshot } from "../types";
import FieldHint from "./FieldHint.vue";

const props = defineProps<{
    /** 后端通道（与平台编辑器注入的是同一个实例） */
    backend: Backend | null;
}>();

const emit = defineEmits<{
    (e: "close"): void;
    (e: "notify", text: string, isError?: boolean): void;
}>();

/** Oracle 连接信息（打开时由服务端快照回填） */
const oracle = reactive<OracleConnectConfig>({
    envName: "",
    connectString: "",
    user: "",
    password: "",
});
/** 资金账号 */
const accountId = ref("");
/** 当前表格中的持仓（查询结果或服务端已保存的数据） */
const rows = ref<PositionRow[]>([]);
/** 服务端是否已保存过持仓（决定提示文字） */
const synced = ref(false);
/** 打开时加载快照中 */
const loading = ref(false);
/** 查询/保存进行中（按钮置灰防连点） */
const querying = ref(false);
const saving = ref(false);
/** 错误提示（连接失败/查询失败等，显示在表单下方） */
const error = ref("");
/** 查询过但还没保存（表格下方提示用） */
const queried = ref(false);

const COLUMNS: { key: keyof PositionRow; label: string; numeric: boolean; width: string }[] = [
    { key: "securityId", label: "证券代码", numeric: false, width: "110px" },
    { key: "securityName", label: "证券名称", numeric: false, width: "180px" },
    { key: "holdingQty", label: "持有数量", numeric: true, width: "130px" },
    { key: "availableQty", label: "可用数量", numeric: true, width: "130px" },
    { key: "creationRedemptionQty", label: "可申赎数量", numeric: true, width: "150px" },
    { key: "frozenQty", label: "冻结数量", numeric: true, width: "130px" },
];

onMounted(async () => {
    if (!props.backend) {
        error.value = "后端未连接，无法加载持仓数据";
        return;
    }
    loading.value = true;
    const resp = await props.backend.dispatch({ cmd: "get_position_state" });
    loading.value = false;
    if (!resp.ok) {
        error.value = resp.error ?? "加载服务端持仓失败";
        return;
    }
    const snap = resp.data as PositionSnapshot;
    Object.assign(oracle, snap.oracle ?? {});
    accountId.value = snap.accountId ?? "";
    rows.value = (snap.rows ?? []).map((r) => ({ ...r }));
    synced.value = !!snap.synced;
});

/** 查询持仓：结果只更新表格，点“保存”后才写入服务端内存 */
async function queryPositions() {
    error.value = "";
    if (!props.backend) {
        error.value = "后端未连接，无法查询持仓";
        return;
    }
    if (!oracle.connectString.trim()) {
        error.value = "请填写 Oracle 连接串";
        return;
    }
    if (!oracle.user.trim()) {
        error.value = "请填写 Oracle 用户名";
        return;
    }
    if (!accountId.value.trim()) {
        error.value = "请填写资金账号";
        return;
    }
    querying.value = true;
    const resp = await props.backend.dispatch({
        cmd: "query_positions",
        oracle: {
            envName: oracle.envName.trim(),
            connectString: oracle.connectString.trim(),
            user: oracle.user.trim(),
            password: oracle.password,
        },
        accountId: accountId.value.trim(),
    });
    querying.value = false;
    if (!resp.ok) {
        error.value = resp.error ?? "查询持仓失败";
        return;
    }
    rows.value = ((resp.data as PositionRow[]) ?? []).map((r) => ({ ...r }));
    queried.value = true;
    emit("notify", `查询到 ${rows.value.length} 条持仓（修改后点“保存”生效）`);
}

/** 保存：整份替换服务端内存中的持仓，成功后关闭弹窗 */
async function savePositions() {
    error.value = "";
    if (!props.backend) {
        error.value = "后端未连接，无法保存持仓";
        return;
    }
    if (!accountId.value.trim()) {
        error.value = "请填写资金账号";
        return;
    }
    // 空证券代码的行没有意义（查询结果不会出现），保存时过滤掉
    const normalized: PositionRow[] = rows.value
        .filter((r) => r.securityId.trim() !== "")
        .map((r) => ({
            securityId: r.securityId.trim(),
            securityName: r.securityName.trim(),
            holdingQty: Number(r.holdingQty) || 0,
            availableQty: Number(r.availableQty) || 0,
            creationRedemptionQty: Number(r.creationRedemptionQty) || 0,
            frozenQty: Number(r.frozenQty) || 0,
        }));
    saving.value = true;
    const resp = await props.backend.dispatch({
        cmd: "save_positions",
        oracle: {
            envName: oracle.envName.trim(),
            connectString: oracle.connectString.trim(),
            user: oracle.user.trim(),
            password: oracle.password,
        },
        accountId: accountId.value.trim(),
        rows: normalized,
    });
    saving.value = false;
    if (!resp.ok) {
        error.value = resp.error ?? "保存持仓失败";
        return;
    }
    emit("notify", `已保存 ${normalized.length} 条持仓数据，ETF 申购将按此扣减`);
    emit("close");
}
</script>

<template>
    <!-- 叠在平台编辑弹窗之上的二级弹窗：z-index 比 .modal-mask 高一档 -->
    <div class="modal-mask" style="z-index: 120" @mousedown.self="emit('close')">
        <div class="modal pos-modal">
            <div class="modal-head">
                <span>同步持仓数据</span>
                <button class="modal-close" @click="emit('close')">✕</button>
            </div>

            <div class="modal-body">
                <!-- 连接信息：两列网格；连接串占整行（一般较长） -->
                <div class="pos-grid">
                    <div class="field">
                        <label>环境名</label>
                        <input v-model="oracle.envName" type="text" placeholder="如 qa环境" />
                    </div>
                    <div class="field">
                        <label>资金账号</label>
                        <input v-model="accountId" type="text" placeholder="如 60500052" />
                    </div>
                    <div class="field full">
                        <label>连接串</label>
                        <input
                            v-model="oracle.connectString"
                            type="text"
                            placeholder="如 10.210.50.63:2151/spdtrderbak"
                        />
                    </div>
                    <div class="field">
                        <label>用户名</label>
                        <input v-model="oracle.user" type="text" placeholder="如 QA_SPEEDTRADER" />
                    </div>
                    <div class="field">
                        <label>密码</label>
                        <input v-model="oracle.password" type="password" placeholder="Oracle 登录密码" />
                    </div>
                </div>

                <!-- 查询按钮与提示：说明该功能的使用规则 -->
                <div class="pos-actions">
                    <button class="btn primary" :disabled="querying || loading" @click="queryPositions">
                        {{ querying ? "查询中…" : "查询持仓" }}
                    </button>
                    <span class="hint">
                        查询到的“可申赎数量”将作为 ETF 申购的成份股库存；保存后替换服务端内存中的唯一一份持仓
                    </span>
                    <FieldHint right text="用于作为成分股进行ETF申购" />
                </div>

                <div v-if="error" class="pos-error">{{ error }}</div>
                <div v-if="loading" class="hint">正在加载服务端持仓…</div>

                <!-- 持仓表格：六列均可编辑，纵向滚动，表头吸顶 -->
                <div class="pos-table-wrap">
                    <table class="pos-table">
                        <thead>
                            <tr>
                                <th
                                    v-for="c in COLUMNS"
                                    :key="c.key"
                                    :style="{ width: c.width }"
                                >{{ c.label }}</th>
                            </tr>
                        </thead>
                        <tbody>
                            <tr v-for="(r, i) in rows" :key="i">
                                <td v-for="c in COLUMNS" :key="c.key">
                                    <input
                                        v-if="c.numeric"
                                        v-model.number="r[c.key]"
                                        type="number"
                                        step="1"
                                    />
                                    <input v-else v-model="r[c.key]" type="text" />
                                </td>
                            </tr>
                            <tr v-if="!rows.length">
                                <td :colspan="COLUMNS.length" class="pos-empty">
                                    暂无持仓：填写连接信息与资金账号后点击“查询持仓”
                                </td>
                            </tr>
                        </tbody>
                    </table>
                </div>

                <!-- 数据来源提示：区分“内存中已保存”与“刚查询未保存” -->
                <div class="hint pos-status">
                    <template v-if="queried">查询结果 {{ rows.length }} 条（尚未保存）</template>
                    <template v-else-if="synced">
                        服务端内存中已有 {{ rows.length }} 条持仓（上次保存的数据）
                    </template>
                    <template v-else>服务端尚未保存过持仓（ETF 申购不扣减）</template>
                </div>
            </div>

            <div class="modal-foot">
                <button class="btn" @click="emit('close')">取消</button>
                <button class="btn primary" :disabled="saving || loading" @click="savePositions">
                    {{ saving ? "保存中…" : "保存" }}
                </button>
            </div>
        </div>
    </div>
</template>

<style scoped>
/* 表格较宽，弹窗固定大宽度；最大高度仍受全局 .modal 的 88vh 限制 */
.pos-modal {
    width: 980px;
}

/* 连接信息两列网格（连接串占整行） */
.pos-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px 16px;
}
.pos-grid .field.full {
    grid-column: 1 / -1;
}

.pos-actions {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 14px 0 8px;
    flex-wrap: wrap;
}
.pos-actions .hint {
    flex: 1;
    min-width: 240px;
}

.pos-error {
    margin: 6px 0;
    padding: 7px 10px;
    border: 1px solid var(--red);
    border-radius: var(--radius-sm);
    color: var(--red);
    font-size: 13px;
    background: var(--red-soft);
    white-space: pre-wrap;
}

/* 表格滚动区：限制高度，表头吸顶 */
.pos-table-wrap {
    margin-top: 8px;
    max-height: 44vh;
    overflow: auto;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
}
.pos-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
}
.pos-table th {
    position: sticky;
    top: 0;
    z-index: 1;
    background: var(--bg-soft);
    color: var(--accent);
    font-weight: 600;
    text-align: left;
    padding: 7px 8px;
    border-bottom: 1px solid var(--border);
    white-space: nowrap;
}
.pos-table td {
    padding: 3px 5px;
    border-bottom: 1px solid var(--border);
}
.pos-table tr:last-child td {
    border-bottom: none;
}
.pos-table input {
    width: 100%;
    box-sizing: border-box;
    padding: 5px 7px;
    border: 1px solid transparent;
    border-radius: 4px;
    background: transparent;
    color: var(--text);
    font-size: 13px;
}
.pos-table input:hover,
.pos-table input:focus {
    border-color: var(--border);
    background: var(--panel);
    outline: none;
}
.pos-empty {
    text-align: center;
    color: var(--text-faint);
    padding: 26px 0 !important;
}
.pos-status {
    margin-top: 8px;
}
</style>
