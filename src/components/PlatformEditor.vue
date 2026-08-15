<script setup lang="ts">
// PlatformEditor.vue —— 平台配置的编辑弹窗（新建和编辑共用）。
// 表单项随策略模式动态显隐：选“拆单”才显示拆单参数、
// 选“自定义”才显示成交明细、选“拒单”才显示拒单参数。
// 点“保存”时先把数值规范化，再把整份配置 emit 给父组件提交。
import { computed, reactive, watch } from "vue";
import type { CustomFill, GatewayCategory, PlatformConfig } from "../types";
import { platformTypesOf, STRATEGY_MODES, defaultRejectReason } from "../types";
import { rejectTextOf } from "../errors";

const props = defineProps<{
    /** 待编辑的平台配置（新建时传默认值） */
    platform: PlatformConfig;
    /** 所属网关分类（决定平台类型列表与是否显示密码校验） */
    category: GatewayCategory;
    /** true = 新建，false = 编辑（只影响标题文字） */
    isNew: boolean;
}>();

// 两个出口：保存（带着改好的配置）/ 关闭（什么都不做）
const emit = defineEmits<{
    (e: "save", p: PlatformConfig): void;
    (e: "close"): void;
}>();

// 深拷贝一份进行编辑，取消时不影响原数据
const form = reactive<PlatformConfig>(JSON.parse(JSON.stringify(props.platform)));

// 父组件换了一个平台进来时，把表单内容整体替换成新的
watch(
    () => props.platform,
    (p) => {
        Object.assign(form, JSON.parse(JSON.stringify(p)));
        initRejectDefaults();
    },
);

/** 拒单模式下补全默认值：原因码为空或还是旧默认 1 时，换成该网关分类的
 *  默认码（深圳 20009 / 上海 1025），并按错误码表映射说明文本 */
function initRejectDefaults() {
    if (form.strategy.mode !== "reject") return;
    if (!form.strategy.rejectReason || form.strategy.rejectReason === 1) {
        form.strategy.rejectReason = defaultRejectReason(props.category);
    }
    applyRejectText();
}

initRejectDefaults();

// 只有两种拆单模式需要“拆单笔数”和“价格档位”这两组参数
const needSplit = () => form.strategy.mode === "fullSplit" || form.strategy.mode === "partialSplit";
const needTick = () => needSplit();

/** 当前拒单原因码在错误码表里的说明（拒单参数区的提示文字用） */
const rejectTextHint = computed(() => rejectTextOf(props.category, form.strategy.rejectReason));

// 按网关分类选用平台类型列表（上交所竞价仅“竞价平台”、新债券仅“新债券平台”）
const platformTypes = computed(() => platformTypesOf(props.category));
// 上交所协议（竞价/新债券）Logon 无密码字段，故仅深交所网关显示密码校验选项
const showPassword = computed(() => props.category === "sz");

/** 自定义成交明细：加一笔（默认 100 股 / 10 元） */
function addFill() {
    form.strategy.customFills.push({ qty: 100, price: 10 });
}

/** 自定义成交明细：删掉第 i 笔 */
function removeFill(i: number) {
    form.strategy.customFills.splice(i, 1);
}

// ---- 拒单参数的默认值联动（按网关分类） ----
// 选“拒单”时若原因码还是旧默认值 1，自动换成该分类的默认码
// （深圳 20009 / 上海 1025）；原因码变化时按错误码表自动映射
// 说明文本（表里没有的代码保留手动输入，不覆盖）
watch(
    () => form.strategy.mode,
    (m) => {
        if (m === "reject") initRejectDefaults();
    },
);
watch(
    () => form.strategy.rejectReason,
    () => {
        if (form.strategy.mode === "reject") applyRejectText();
    },
);

/** 按错误码表把说明文本同步成原因码对应的内容（表里没有则不动） */
function applyRejectText() {
    const t = rejectTextOf(props.category, form.strategy.rejectReason);
    if (t) form.strategy.rejectText = t;
}

/** 点“保存”：先把各数值整理成合法值，再交给父组件提交后端 */
function submit() {
    if (!form.name.trim()) {
        form.name = "未命名平台";
    }
    // 规范化数值，避免空输入：
    // 笔数至少为 1 且取整；各区间保证“最大 ≥ 最小”，
    // 不然后端在区间内随机取值时会出错
    const s = form.strategy;
    s.splitCountMin = Math.max(1, Math.floor(s.splitCountMin || 1));
    s.splitCountMax = Math.max(s.splitCountMin, Math.floor(s.splitCountMax || s.splitCountMin));
    s.ackDelay.minMs = Math.max(0, s.ackDelay.minMs || 0);
    s.ackDelay.maxMs = Math.max(s.ackDelay.minMs, s.ackDelay.maxMs || 0);
    s.tradeDelay.minMs = Math.max(0, s.tradeDelay.minMs || 0);
    s.tradeDelay.maxMs = Math.max(s.tradeDelay.minMs, s.tradeDelay.maxMs || 0);
    // 数量或价格填了 0 的明细行直接丢弃；自定义模式下全删光了就补一笔默认的
    s.customFills = s.customFills.filter((f: CustomFill) => f.qty > 0 && f.price > 0);
    if (s.mode === "custom" && s.customFills.length === 0) {
        s.customFills = [{ qty: 100, price: 10 }];
    }
    emit("save", JSON.parse(JSON.stringify(form)));
}
</script>

<template>
    <!-- @mousedown.self：只有点在遮罩本身（弹窗外的暗区）才关闭，点弹窗内部不会 -->
    <div class="modal-mask" @mousedown.self="emit('close')">
        <div class="modal">
            <div class="modal-head">
                <span>{{ isNew ? "新建平台" : "编辑平台" }}</span>
                <button class="modal-close" @click="emit('close')">✕</button>
            </div>
            <div class="modal-body">
                <div class="form-grid">
                    <!-- 基础信息：名称/类型/监听地址端口/CompID/分区号/密码 -->
                    <div class="field">
                        <label>平台名称</label>
                        <input v-model="form.name" placeholder="如：现货集中竞价交易平台" />
                    </div>
                    <div class="field">
                        <label>平台类型 (PlatformID)</label>
                        <select v-model.number="form.platformType">
                            <option v-for="t in platformTypes" :key="t.value" :value="t.value">
                                {{ t.value }} - {{ t.label }}
                            </option>
                        </select>
                    </div>
                    <div class="field">
                        <label>监听地址</label>
                        <input v-model="form.listenHost" placeholder="0.0.0.0" />
                    </div>
                    <div class="field">
                        <label>监听端口</label>
                        <input v-model.number="form.port" type="number" min="1" max="65535" />
                    </div>
                    <div class="field">
                        <label>网关 CompID（回报中的 SenderCompID）</label>
                        <input v-model="form.compId" maxlength="20" />
                    </div>
                    <div class="field">
                        <label>平台分区号 (PartitionNo)</label>
                        <input v-model.number="form.partitionNo" type="number" min="0" />
                    </div>
                    <div class="field" v-if="showPassword">
                        <label class="field-row" style="margin-top: 20px">
                            <input v-model="form.checkPassword" type="checkbox" />
                            校验登录密码
                        </label>
                    </div>
                    <div class="field" v-if="showPassword && form.checkPassword">
                        <label>登录密码 (Password)</label>
                        <input v-model="form.password" />
                    </div>

                    <!-- 平台功能开关：展示收发报文（勾选即自动持久化）+ 缓存订单。
                         两个开关并列一行，不再分组，界面更简洁 -->
                    <div class="field full">
                        <div class="field-row" style="gap: 24px">
                            <label class="field-row">
                                <input v-model="form.showPackets" type="checkbox" />
                                展示收发报文
                            </label>
                            <label class="field-row">
                                <input v-model="form.cacheOrders" type="checkbox" />
                                缓存订单
                            </label>
                        </div>
                        <span class="hint">
                            展示收发报文：点击平台卡片“报文”可实时查看并按字段解析，同时自动持久化到 packets/ 目录;
                            缓存订单：可查看订单列表，撤单按真实状态回执
                        </span>
                    </div>

                    <div class="section-title">模拟回报策略</div>

                    <!-- 回报模式下拉框；选不同模式下面显示不同的参数区 -->
                    <div class="field full">
                        <label>回报模式</label>
                        <select v-model="form.strategy.mode">
                            <option v-for="m in STRATEGY_MODES" :key="m.value" :value="m.value">
                                {{ m.label }}
                            </option>
                        </select>
                    </div>

                    <!-- 拆单类模式的参数：拆几笔、逐笔价差 -->
                    <template v-if="needSplit()">
                        <div class="field">
                            <label>拆单笔数（最小 ~ 最大，随机）</label>
                            <div class="field-row">
                                <input v-model.number="form.strategy.splitCountMin" type="number" min="1" />
                                <span class="hint">~</span>
                                <input v-model.number="form.strategy.splitCountMax" type="number" min="1" />
                            </div>
                        </div>
                    </template>
                    <template v-if="needTick()">
                        <div class="field">
                            <label>价格档位（每笔递增/递减，元）</label>
                            <input v-model.number="form.strategy.priceTick" type="number" step="0.001" min="0" />
                            <span class="hint">买单从委托价逐档递减，卖单逐档递增</span>
                        </div>
                    </template>

                    <!-- 自定义模式：逐笔列出成交的数量和价格，可增删 -->
                    <template v-if="form.strategy.mode === 'custom'">
                        <div class="field full">
                            <label>自定义成交明细（数量 / 价格）</label>
                            <div v-for="(f, i) in form.strategy.customFills" :key="i" class="field-row" style="margin-bottom: 6px">
                                <input v-model.number="f.qty" type="number" min="1" placeholder="数量" />
                                <input v-model.number="f.price" type="number" step="0.001" min="0" placeholder="价格" />
                                <button class="btn sm danger-ghost" @click="removeFill(i)">✕</button>
                            </div>
                            <button class="btn sm" @click="addFill">+ 添加一笔</button>
                        </div>
                    </template>

                    <!-- 拒单模式：选拒单方式、原因码、说明文字 -->
                    <template v-if="form.strategy.mode === 'reject'">
                        <div class="field">
                            <label>拒单方式</label>
                            <select v-model="form.strategy.rejectVia">
                                <option value="executionReport">确认回报 (ExecType=8 拒绝)</option>
                                <option value="businessReject">业务拒绝消息 (BusinessReject)</option>
                            </select>
                        </div>
                        <div class="field">
                            <label>拒单原因码 (OrdRejReason)</label>
                            <input v-model.number="form.strategy.rejectReason" type="number" min="0" />
                            <span class="hint" v-if="rejectTextHint">
                                默认 {{ defaultRejectReason(props.category) }}（{{ rejectTextHint }}）
                            </span>
                            <span class="hint" v-else>默认 {{ defaultRejectReason(props.category) }}</span>
                        </div>
                        <div class="field full">
                            <label>拒单说明文本</label>
                            <input v-model="form.strategy.rejectText" maxlength="50" />
                        </div>
                    </template>

                    <!-- 回报延迟：拒单模式没有成交，所以不显示；只挂单模式只有确认延迟 -->
                    <template v-if="form.strategy.mode !== 'reject'">
                        <div class="section-title">回报延迟（毫秒，最小 = 最大为固定延迟，全 0 同步回报）</div>
                        <div class="field">
                            <label>确认回报延迟（最小 ~ 最大）</label>
                            <div class="field-row">
                                <input v-model.number="form.strategy.ackDelay.minMs" type="number" min="0" />
                                <span class="hint">~</span>
                                <input v-model.number="form.strategy.ackDelay.maxMs" type="number" min="0" />
                            </div>
                        </div>
                        <div class="field" v-if="form.strategy.mode !== 'ackOnly'">
                            <label>成交回报延迟（最小 ~ 最大）</label>
                            <div class="field-row">
                                <input v-model.number="form.strategy.tradeDelay.minMs" type="number" min="0" />
                                <span class="hint">~</span>
                                <input v-model.number="form.strategy.tradeDelay.maxMs" type="number" min="0" />
                            </div>
                        </div>
                    </template>
                </div>
            </div>
            <div class="modal-foot">
                <button class="btn" @click="emit('close')">取消</button>
                <button class="btn primary" @click="submit">保存</button>
            </div>
        </div>
    </div>
</template>
