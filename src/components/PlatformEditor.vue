<script setup lang="ts">
// PlatformEditor.vue —— 平台配置的编辑弹窗（新建和编辑共用）。
// 报单自动回报模式为两级选择：一级 5 大类（固定回报 / Lua 脚本配置 /
// 回放行情撮合（待开发，暂按关闭自动回报保存）/ 关闭自动回报，选”固定回报”
// 时右侧出现具体的 6 种子模式下拉。分类只是界面层归类，strategy.mode 仍
// 写后端既有的枚举值，gateways.json 格式不变。
// Lua 脚本配置模式下只有一个输入框（原 Lua 编辑器与 AI 输入框的合并）：
// 默认放一段需求示例（DEFAULT_LUA_PROMPT），点「AI 生成脚本」把它变成
// 真正的 Lua 脚本填回同一处；保存时后端强制校验语法。
// 回报延迟拆到两个模式区块下：确认/成交回报延迟放在"报单自动回报模式"
// 区块的最后（拆单模式时在拆单笔数/价格档位之下；自动拒单也在最底部只显示
// "确认回报延迟"——拒单回报同样按该配置生成延迟）。仅"固定回报"大类显示，
// 无成交的只回确认不显示"成交回报延迟"；Lua 模式延迟只能由脚本 delayMs
// 控制，界面不参与（保存时自动把确认/成交延迟归零）；
// "撤单回报延迟"放在"撤单自动回报模式"区块下、任何模式都显示——脚本不接管
// 撤单，撤单延迟走界面撤单模式配置。
// 网关运行中编辑时：基础信息（名称/类型/地址/端口/CompID/分区号）与
// 功能开关（展示收发报文/缓存订单）置灰不可改、不显示登录校验密码区，
// 仅”报单自动回报策略 / 撤单自动回报策略 / 回报延迟”可改（后端热更新实时生效）。
// 点”保存”时先把数值规范化，再把整份配置 emit 给父组件提交。
import { computed, reactive, ref, watch } from "vue";
import type { GatewayCategory, PlatformConfig, StrategyCategory, StrategyMode } from "../types";
import type { Backend } from "../backend";
import {
    CANCEL_MODES,
    DEFAULT_LUA_PROMPT,
    defaultDbConfig,
    defaultPartitionNos,
    defaultRejectReason,
    FIXED_STRATEGY_MODES,
    platformTypesOf,
    STRATEGY_CATEGORIES,
    strategyCategoryOf,
} from "../types";
import LuaEditor from "./LuaEditor.vue";
import FieldHint from "./FieldHint.vue";
import LuaDocViewer from "./LuaDocViewer.vue";
import PositionSyncDialog from "./PositionSyncDialog.vue";
import { generateLuaScript } from "../llm";
import { rejectTextOf } from "../errors";

const props = defineProps<{
    /** 待编辑的平台配置（新建时传默认值） */
    platform: PlatformConfig;
    /** 所属网关分类（决定平台类型列表与是否显示密码校验） */
    category: GatewayCategory;
    /** true = 新建，false = 编辑（只影响标题文字） */
    isNew: boolean;
    /** 所属网关是否运行中（运行中基础字段置灰、隐藏密码区，仅策略可热更新） */
    gatewayRunning: boolean;
    /** 后端通道（"检查脚本"按钮即时校验用；与 OrderViewer 同样的注入方式） */
    backend: Backend | null;
}>();

// 四个出口：保存 / 关闭 / notify（检查结果转 toast）/ scriptError（AI 生成
// 的"不支持/需补充/失败"提示走带标题的详情弹窗，由父组件打开）
const emit = defineEmits<{
    (e: "save", p: PlatformConfig): void;
    (e: "close"): void;
    (e: "notify", text: string, isError?: boolean): void;
    (e: "scriptError", text: string, title?: string): void;
}>();

// 深拷贝一份进行编辑，取消时不影响原数据；
// 策略字段做兜底：旧版后端/旧配置返回的策略可能缺新增字段（撤单模式/
// 撤单延迟等），缺了就补默认值，避免模板访问 undefined 报错
const form = reactive<PlatformConfig>(normalizePlatform(props.platform));

/** 补全平台配置里的策略字段缺省值（向后兼容旧数据） */
function normalizePlatform(p: PlatformConfig): PlatformConfig {
    const s = p.strategy ?? {};
    const d = { minMs: 0, maxMs: 0 };
    return {
        ...p,
        db: p.db ?? (props.category === "shzh" ? defaultDbConfig() : undefined),
        strategy: {
            mode: s.mode ?? "fullSingle",
            splitCountMin: s.splitCountMin ?? 2,
            splitCountMax: s.splitCountMax ?? 5,
            priceTick: s.priceTick ?? 0.01,
            rejectVia: props.category === "shzh" ? "executionReport" : (s.rejectVia ?? "executionReport"),
            rejectReason: s.rejectReason ?? 1,
            rejectText: s.rejectText ?? "模拟拒单",
            ackDelay: s.ackDelay ?? { ...d },
            tradeDelay: s.tradeDelay ?? { ...d },
            cancelMode: s.cancelMode ?? "default",
            cancelRejectReason: s.cancelRejectReason ?? 1,
            cancelFrontReject: props.category === "shzh" ? false : (s.cancelFrontReject ?? false),
            cancelDelay: s.cancelDelay ?? { ...d },
            luaScript: s.luaScript ?? "",
            etfPcfDir: s.etfPcfDir ?? "",
        },
    };
}

// 父组件换了一个平台进来时，把表单内容整体替换成新的
watch(
    () => props.platform,
    (p) => {
        Object.assign(form, normalizePlatform(p));
        initRejectDefaults();
        // 换平台后重置"上次固定模式"，避免沿用上一个平台的选择
        lastFixedMode.value =
            strategyCategoryOf(form.strategy.mode) === "fixed" ? form.strategy.mode : "fullSingle";
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

// ---- 报单自动回报的"一级分类"（4 大类）与 strategy.mode 的双向映射 ----
// 一级下拉只是界面归类，真正落到配置里的还是后端既有的 8 个枚举值：
// 固定回报 → 上次选的固定子模式；Lua 脚本配置 → luaScript；
// 关闭自动回报 → noAutoReply；回放行情撮合（待开发）暂与关闭自动回报
// 同行为（也写 noAutoReply），避免把后端不认识的枚举值写进配置。
// 记住最近一次选的固定子模式，一级分类从"固定回报"切走再切回时沿用；
// 初始值取当前配置的模式（若是固定模式），避免编辑已有平台时切去 Lua
// 再切回来，把用户原本选的子模式悄悄改成全部成交
const lastFixedMode = ref<StrategyMode>(
    strategyCategoryOf(form.strategy.mode) === "fixed" ? form.strategy.mode : "fullSingle",
);
const strategyCategory = computed<StrategyCategory>({
    get: () => strategyCategoryOf(form.strategy.mode),
    set: (c: StrategyCategory) => {
        if (c === "fixed") form.strategy.mode = lastFixedMode.value;
        else if (c === "luaScript") form.strategy.mode = "luaScript";
        else if (c === "off" || c === "replay") form.strategy.mode = "noAutoReply";
        // 回放行情撮合（待开发）：暂时与"关闭自动回报"同行为
    },
});

// 只有"固定回报"里的拆单两种模式需要"拆单笔数"和"价格档位"这两组参数
const needSplit = () => form.strategy.mode === "fullSplit" || form.strategy.mode === "partialSplit";
const needTick = () => needSplit();

// 按网关分类选用平台类型列表（上交所竞价仅“竞价平台”、新债券仅“新债券平台”）
const platformTypes = computed(() => platformTypesOf(props.category));
// 上交所协议（竞价/新债券）Logon 无密码字段，故仅深交所网关显示密码校验选项
const showPassword = computed(() => props.category === "sz");
/** 上海综合网关：走数据库接口，界面显示数据库字段而不是监听地址/端口 */
const isDb = computed(() => props.category === "shzh");

/** 自定义成交已删除（有手动回复功能替代），无需 addFill/removeFill */

// ---- 新建平台：平台类型变化时联动平台名称与分区号默认值 ----
// 名称 = 平台类型的中文名（如选“综合金融服务平台”）；分区号按平台类型给默认值；
// 只在新建场景联动，编辑场景用户改类型不覆盖已填内容
watch(
    () => form.platformType,
    (t) => {
        if (!props.isNew) return;
        const label = platformTypesOf(props.category).find((p) => p.value === t)?.label;
        if (label) form.name = label;
        form.partitionNos = defaultPartitionNos(t, props.category);
    },
);

// ---- 拒单参数的默认值联动（按网关分类） ----
// 选“拒单”时若原因码还是旧默认值 1，自动换成该分类的默认码
// （深圳 20009 / 上海 1025）；原因码变化时按错误码表自动映射
// 说明文本（表里没有的代码保留手动输入，不覆盖）
watch(
    () => form.strategy.mode,
    (m) => {
        // 记住最近一次选的固定子模式，一级分类切回"固定回报"时沿用
        if (strategyCategoryOf(m) === "fixed") lastFixedMode.value = m;
        if (m === "reject") initRejectDefaults();
        // 切到 Lua 脚本配置且输入为空（旧配置没有脚本字段）时预填需求示例，
        // 降低上手门槛；用户清空后再切走/切回不会反复覆盖（只在为空时填）
        if (m === "luaScript" && !form.strategy.luaScript) {
            form.strategy.luaScript = DEFAULT_LUA_PROMPT;
        }
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

/** 点“保存”：先拦截不可保存的状态（Lua 框里还是示例提示词），
 *  再规范化数值，最后交给父组件提交后端 */
function submit() {
    if (
        form.strategy.mode === "luaScript" &&
        form.strategy.luaScript.trim() === DEFAULT_LUA_PROMPT.trim()
    ) {
        emit(
            "notify",
            "当前只是示例提示词：请先点击「AI 生成脚本」把它变成 Lua 脚本，或直接粘贴脚本后再保存",
            true,
        );
        return;
    }
    if (!form.name.trim()) {
        form.name = "未命名平台";
    }
    // 规范化数值，避免空输入：
    // 笔数至少为 1 且取整；各区间保证“最大 ≥ 最小”，
    // 不然后端在区间内随机取值时会出错
    const s = form.strategy;
    // Lua 模式下确认/成交延迟不经界面配置（只能由脚本各项 delayMs 控制）：
    // 这里把配置归零，避免旧版本留下的非零值在脚本没写 delayMs 时悄悄兜底生效
    if (s.mode === "luaScript") {
        s.ackDelay.minMs = 0;
        s.ackDelay.maxMs = 0;
        s.tradeDelay.minMs = 0;
        s.tradeDelay.maxMs = 0;
    }
    s.splitCountMin = Math.max(1, Math.floor(s.splitCountMin || 1));
    s.splitCountMax = Math.max(s.splitCountMin, Math.floor(s.splitCountMax || s.splitCountMin));
    s.ackDelay.minMs = Math.max(0, s.ackDelay.minMs || 0);
    s.ackDelay.maxMs = Math.max(s.ackDelay.minMs, s.ackDelay.maxMs || 0);
    s.tradeDelay.minMs = Math.max(0, s.tradeDelay.minMs || 0);
    s.tradeDelay.maxMs = Math.max(s.tradeDelay.minMs, s.tradeDelay.maxMs || 0);
    s.cancelDelay.minMs = Math.max(0, s.cancelDelay.minMs || 0);
    s.cancelDelay.maxMs = Math.max(s.cancelDelay.minMs, s.cancelDelay.maxMs || 0);
    // 上海综合平台没有独立的业务拒绝消息：拒单统一走执行报告拒绝，
    // 撤单拒绝固定回 OrderCancelReject(9)；隐藏的开关在保存时归一化
    if (isDb.value) {
        s.rejectVia = "executionReport";
        s.cancelFrontReject = false;
        const db = (form.db ??= defaultDbConfig());
        db.host = db.host.trim() || "127.0.0.1";
        db.port = Math.max(1, Math.floor(db.port || 1433));
        db.database = db.database.trim();
        db.user = db.user.trim();
        db.reqRespTable = db.reqRespTable.trim() || "reqresp";
        db.execReportTable = db.execReportTable.trim() || "execreport";
        db.pollIntervalMs = Math.max(10, Math.floor(db.pollIntervalMs || 100));
        if (!db.database) {
            emit("notify", "数据库名不能为空", true);
            return;
        }
    }
    emit("save", JSON.parse(JSON.stringify(form)));
}

/** “检查脚本”：调后端 validate_lua_script 即时校验语法（只编译不执行、不保存），
 *  结果经 notify 上报父组件转 toast 展示；保存时的强制校验在后端进行 */
async function checkScript() {
    const resp = await props.backend?.dispatch({
        cmd: "validate_lua_script",
        script: form.strategy.luaScript,
    });
    if (!resp) return;
    if (resp.ok) {
        emit("notify", "Lua 脚本语法正确");
    } else {
        emit("notify", resp.error ?? "脚本语法错误", true);
    }
}

// ---- AI 生成脚本（纯前端调大模型，模拟引擎不参与） ----
/** 生成中标记：按钮转圈文字、期间禁用其他按钮 */
const generating = ref(false);
/** 脚本手册弹窗开关 */
const docShow = ref(false);
/** “同步持仓数据”弹窗开关（ETF 清单路径下方的按钮） */
const posShow = ref(false);

/** 「AI 生成脚本」：把编辑框当前内容当作需求描述交给大模型，按三种结果分别处理——
 *  生成成功（脚本填入同一编辑框，用户原话转为注释头）/ 当前不支持（弹窗说明
 *  具体不支持点）/ 信息不足（弹窗展示模型的追问）。网络或格式异常也弹窗。
 *  编辑框即需求描述入口：默认放的是示例文字（DEFAULT_LUA_PROMPT），
 *  点生成就把它变成 Lua 脚本；想换需求直接改框内文字再点生成即可。
 *  调用大模型前先做一次语法校验：框里已经是合法 Lua 脚本时（用户粘的
 *  或上轮生成的），直接提示不走大模型——避免把现成脚本当需求再生成一遍 */
async function generateScript() {
    const input = form.strategy.luaScript.trim();
    if (!input) {
        emit("notify", "请先在脚本框中输入需求描述或 Lua 脚本文本", true);
        return;
    }
    // 预校验：已合法则提示并结束，不调大模型（默认示例是中文需求描述，
    // 语法非法，不会被误拦截；粘了现成脚本或上轮生成结果时才会命中）
    const check = await props.backend?.dispatch({
        cmd: "validate_lua_script",
        script: input,
    });
    if (check?.ok) {
        emit("notify", "当前已是合法 Lua 脚本，无需再生成；可直接保存或手动修改");
        return;
    }
    generating.value = true;
    try {
        const r = await generateLuaScript(input);
        if (r.type === "script") {
            // 用户逻辑原样作为注释头插到脚本最前（逐行加 -- 前缀；行号
            // 随之下移，但报错行号与编辑器行号始终一致，不影响排错）
            const header = input
                .split("\n")
                .map((line) => "-- " + line)
                .join("\n");
            form.strategy.luaScript = header + "\n" + r.script;
            emit("notify", "脚本已生成，可继续手动调整");
        } else if (r.type === "unsupported") {
            emit("scriptError", r.reason, "当前不支持该功能");
        } else {
            // clarify：信息不足或描述矛盾，弹窗展示模型的追问/矛盾点
            emit("scriptError", r.question, "AI 需要澄清");
        }
    } catch (e) {
        emit("scriptError", String(e), "AI 生成失败");
    } finally {
        generating.value = false;
    }
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
                    <!-- 基础信息：名称/类型/监听地址端口/CompID/分区号（网关运行中置灰，
                         仅策略与回报延迟可热更新，其余字段需停止网关后才能改） -->
                    <div class="field">
                        <label>平台名称</label>
                        <input v-model="form.name" placeholder="如：现货集中竞价交易平台" :disabled="gatewayRunning" />
                    </div>
                    <div class="field">
                        <label>平台类型 (PlatformID)</label>
                        <select v-model.number="form.platformType" :disabled="gatewayRunning">
                            <option v-for="t in platformTypes" :key="t.value" :value="t.value">
                                {{ t.value }} - {{ t.label }}
                            </option>
                        </select>
                    </div>
                    <!-- 上海综合网关：数据库连接与接口表配置（不走 TCP） -->
                    <template v-if="isDb && form.db">
                        <div class="field">
                            <label>数据库主机</label>
                            <input v-model="form.db.host" placeholder="如 127.0.0.1" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label>数据库端口</label>
                            <input
                                v-model.number="form.db.port"
                                type="number"
                                min="1"
                                max="65535"
                                :disabled="gatewayRunning"
                            />
                        </div>
                        <div class="field">
                            <label>数据库名</label>
                            <input v-model="form.db.database" placeholder="如 ezstep" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label>数据库账号</label>
                            <input v-model="form.db.user" placeholder="如 sa" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label>数据库密码</label>
                            <input v-model="form.db.password" type="password" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label class="field-row">
                                委托表名
                                <FieldHint text="请求及响应接口表（EzSTEP 标准表名 reqresp）：柜台写入委托、网关回写确认" />
                            </label>
                            <input v-model="form.db.reqRespTable" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label class="field-row">
                                成交表名
                                <FieldHint right text="执行报告表（EzSTEP 标准表名 execreport）：网关把成交回报插入该表" />
                            </label>
                            <input v-model="form.db.execReportTable" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label class="field-row">
                                轮询间隔（毫秒）
                                <FieldHint text="每隔多久查一次委托表里 ordstatus='R' 的新委托；建议 50~500" />
                            </label>
                            <input
                                v-model.number="form.db.pollIntervalMs"
                                type="number"
                                min="10"
                                max="60000"
                                :disabled="gatewayRunning"
                            />
                        </div>
                    </template>

                    <template v-if="!isDb">
                        <div class="field">
                            <label>监听地址</label>
                            <input v-model="form.listenHost" placeholder="0.0.0.0" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label>监听端口</label>
                            <input v-model.number="form.port" type="number" min="1" max="65535" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label>网关 CompID（回报中的 SenderCompID）</label>
                            <input v-model="form.compId" maxlength="20" :disabled="gatewayRunning" />
                        </div>
                        <div class="field">
                            <label>平台分区号 (PartitionNo)</label>
                            <input v-model="form.partitionNos" placeholder="如 115,116,117,118（逗号分隔）" :disabled="gatewayRunning" />
                        </div>
                    </template>
                    <!-- 登录校验密码（仅网关停止/新建时显示；运行中隐藏）：
                         勾选框与密码输入框横向同一排；勾选后输入框才出现 -->
                    <div class="field full" v-if="showPassword && !gatewayRunning">
                        <div class="field-row">
                            <label class="field-row">
                                <input v-model="form.checkPassword" type="checkbox" />
                                校验登录密码
                            </label>
                            <label class="field-row" v-if="form.checkPassword">
                                登录密码
                                <input v-model="form.password" />
                            </label>
                        </div>
                    </div>

<!-- 平台功能开关：展示收发报文（勾选即自动持久化）+ 缓存订单，
                         同一排显示（不受密码区影响）；网关运行中置灰。
                         说明文字改为问号图标（FieldHint）：悬停/点击查看 -->
                    <div class="field">
                        <label class="field-row">
                            <input v-model="form.showPackets" type="checkbox" :disabled="gatewayRunning" />
                            展示收发报文
                            <FieldHint text="点击平台卡片“报文”可实时查看并按字段解析，同时自动持久化到 packets/ 目录" />
                        </label>
                    </div>
                    <div class="field">
                        <label class="field-row">
                            <input v-model="form.cacheOrders" type="checkbox" :disabled="gatewayRunning" />
                            缓存订单
                            <FieldHint right text="可查看订单列表，撤单按真实状态回执，并支持界面手动回复" />
                        </label>
                    </div>

<!-- 报单自动回报模式区块：大标题旁带问号说明（FieldHint），
                         随后依次是模式选择行、确认/成交回报延迟（仅固定回报模式显示） -->
                    <div class="section-title">
                        报单自动回报模式
                        <FieldHint text="未成交的挂单可以在订单界面手动发送回报" />
                    </div>

                    <!-- 模式选择行：一级 4 大类下拉；选"固定回报模式"时
                         右侧显示具体的 6 种子模式下拉（二级）。一级只是界面归类，
                         strategy.mode 仍写后端既有的枚举值 -->
<div class="field full">
                        <div class="field-row">
                            <select v-model="strategyCategory">
                                <option v-for="c in STRATEGY_CATEGORIES" :key="c.value" :value="c.value">
                                    {{ c.label }}
                                </option>
                            </select>
                            <select v-if="strategyCategory === 'fixed'" v-model="form.strategy.mode">
                                <option v-for="m in FIXED_STRATEGY_MODES" :key="m.value" :value="m.value">
                                    {{ m.label }}
                                </option>
                            </select>
                            <!-- 回放行情撮合（待开发）：与关闭自动回报同行为，无附加参数 -->
                        </div>
                    </div>

                    <!-- 拆单类模式的参数：拆几笔、逐笔价差 -->
                    <template v-if="needSplit()">
                        <div class="field">
                            <label class="field-row">
                                拆单笔数
                                <FieldHint text="最小 ~ 最大之间随机" />
                            </label>
                            <div class="field-row">
                                <input v-model.number="form.strategy.splitCountMin" type="number" min="1" />
                                <span class="hint">~</span>
                                <input v-model.number="form.strategy.splitCountMax" type="number" min="1" />
                            </div>
                        </div>
                    </template>
                    <template v-if="needTick()">
                        <div class="field">
                            <label class="field-row">
                                价格档位
                                <FieldHint right text="每笔价格变动（元）；买单从委托价逐档递减，卖单逐档递增" />
                            </label>
                            <input v-model.number="form.strategy.priceTick" type="number" step="0.001" min="0" />
                        </div>
                    </template>

                    <!-- 拒单模式：选拒单方式、原因码、说明文字 -->
                    <template v-if="form.strategy.mode === 'reject'">
                        <div class="field">
                            <label>拒单方式</label>
                            <select v-model="form.strategy.rejectVia">
                                <option value="executionReport">确认回报 (ExecType=8 拒绝)</option>
                                <option v-if="!isDb" value="businessReject">业务拒绝消息 (BusinessReject)</option>
                            </select>
                        </div>
                        <div class="field">
                            <label>拒单原因码 (OrdRejReason)</label>
                            <input v-model.number="form.strategy.rejectReason" type="number" min="0" />
                        </div>
                        <div class="field full">
                            <label>拒单说明文本</label>
                            <input v-model="form.strategy.rejectText" maxlength="50" />
                        </div>
                    </template>

<!-- Lua 脚本配置：单一输入框（原 Lua 编辑器与 AI 输入框的合并）。
                         默认放一段需求示例（DEFAULT_LUA_PROMPT），点「AI 生成脚本」
                         变成 Lua 脚本并填回同一处；保存时后端强制校验语法 -->
                    <template v-if="form.strategy.mode === 'luaScript'">
                        <div class="field full">
                            <label class="field-row">
                                Lua 回报脚本
                                <FieldHint text='收到新委托时执行，变量 ord 为委托字段、ctx 为环境信息；return "confirm" / "trade" / "reject" 或回报计划表（拒单的方式/原因/文本在脚本里指定），确认/成交回报延迟由脚本各项的 delayMs（毫秒）控制，未写 delayMs 的项同步（0 延迟）发出，完整说明见「脚本手册」' />
                            </label>
                            <LuaEditor v-model="form.strategy.luaScript" :height="240" />
                        </div>
                        <div class="field">
                            <div class="field-row">
                                <button
                                    class="btn sm primary"
                                    type="button"
                                    @click="generateScript"
                                    :disabled="generating"
                                >
                                    {{ generating ? "生成中…" : "AI 生成脚本" }}
                                </button>
                                <button class="btn sm" type="button" @click="checkScript" :disabled="generating">
                                    检查脚本
                                </button>
                                <button class="btn sm" type="button" @click="docShow = true" :disabled="generating">
                                    脚本手册
                                </button>
                            </div>
                        </div>
                    </template>

                    <!-- 确认/成交回报延迟：放在各固定模式参数区之后——拆单模式（随机拆多笔）在
                         拆单笔数/价格档位下面一行，自动拒单在最底部，其余固定模式紧跟模式选择行。
                         自动拒单只回一条拒单回报（无成交），该回报延迟也走"确认回报延迟"配置；
                         只回确认同理无成交 -->
                    <template v-if="strategyCategory === 'fixed'">
                        <div class="field">
                            <label class="field-row">
                                确认回报延迟（毫秒）
                                <FieldHint text="确认回报与拒单回报均适用；最小=最大为固定延迟，否则为区间随机延迟；大于0走定时器异步回报，否则同步回报" />
                            </label>
                            <div class="field-row">
                                <input v-model.number="form.strategy.ackDelay.minMs" type="number" min="0" />
                                <span class="hint">~</span>
                                <input v-model.number="form.strategy.ackDelay.maxMs" type="number" min="0" />
                            </div>
                        </div>
                        <div class="field" v-if="form.strategy.mode !== 'ackOnly' && form.strategy.mode !== 'reject'">
                            <label class="field-row">
                                成交回报延迟（毫秒）
                                <FieldHint right text="最小=最大为固定延迟，否则为区间随机延迟；大于0定时器异步回报，否则同步回报" />
                            </label>
                            <div class="field-row">
                                <input v-model.number="form.strategy.tradeDelay.minMs" type="number" min="0" />
                                <span class="hint">~</span>
                                <input v-model.number="form.strategy.tradeDelay.maxMs" type="number" min="0" />
                            </div>
                        </div>
                    </template>

                    <!-- ETF 清单文件路径：深圳非交易处理平台（平台类型 3）或
                         上海综合平台显示，位置在策略配置区（随策略保存/热更新）。
                         收到 ETF 申赎委托时从该目录按证券代码找日期最新的清单文件：
                         深圳 pcf_<代码>_<日期>.xml；上海 ssepcf_<代码>_<日期>.xml。
                         留空或找不到文件时按 0 只成份券/0 现金替代回复并告警 -->
                    <div
                        class="field full"
                        v-if="(props.category === 'sz' && form.platformType === 3) || props.category === 'shzh'"
                    >
                        <label class="field-row">
                            ETF清单文件路径
                            <FieldHint
                                text="ETF申赎需要从该目录读取清单文件并生成成交明细。深圳文件名 pcf_<代码>_<日期>.xml；上海综合文件名 ssepcf_<代码>_<日期>.xml。留空 = 不启用"
                            />
                        </label>
                        <input
                            v-model="form.strategy.etfPcfDir"
                            type="text"
                            placeholder="如 /opt/simx/pcf（留空 = 不启用，成份券/现金替代按 0 处理）"
                        />
                        <!-- 同步持仓数据：从 Oracle 持仓库按资金账号拉取成份股库存，
                             保存后作为 ETF 申购扣减依据（服务端全局唯一一份） -->
                        <div class="field-row" style="margin-top: 6px">
                            <button class="btn sm" type="button" @click="posShow = true">
                                同步持仓数据
                            </button>
                            <FieldHint right text="用于作为成分股进行ETF申购" />
                        </div>
                    </div>

                    <!-- 撤单自动回报模式区块：撤单模式下拉占满整行（与报单模式行同宽），
                         前台拒单开关与撤单回报延迟在其下一行左右两栏并排；
                         区块标题已含 "撤单" 二字，不再设小标题与 "撤单模式" 标签；
                         前台拒单的说明文字收敛到问号图标（FieldHint） -->
                    <div class="section-title">撤单自动回报模式</div>

                    <!-- 撤单模式：默认（存在挂单就撤单成功、否则拒单）/ 全部撤单成功 /
                         全部撤单拒单（选此项时下方出现原因码输入框） -->
                    <div class="field full">
                        <select v-model="form.strategy.cancelMode">
                            <option v-for="m in CANCEL_MODES" :key="m.value" :value="m.value">
                                {{ m.label }}
                            </option>
                        </select>
                    </div>

                    <!-- 前台拒单与撤单回报延迟左右两栏：开关的说明在问号图标里；
                         延迟对撤单成功与撤单拒单回报均生效、任何报单模式都可配 -->
                    <div class="field" v-if="!isDb">
                        <label class="field-row">
                            <input v-model="form.strategy.cancelFrontReject" type="checkbox" />
                            前台拒单
                            <FieldHint right text="勾选后撤单拒单将改发业务拒绝消息（深交所 MsgType=4 / 上交所 204）" />
                        </label>
                    </div>
                    <div class="field">
                        <label class="field-row">
                            撤单回报延迟（毫秒）
                            <FieldHint right text="撤单成功与撤单拒单回报均适用；最小=最大为固定延迟，否则为区间随机延迟；大于0走定时器异步回报，否则同步回报" />
                        </label>
                        <div class="field-row">
                            <input v-model.number="form.strategy.cancelDelay.minMs" type="number" min="0" />
                            <span class="hint">~</span>
                            <input v-model.number="form.strategy.cancelDelay.maxMs" type="number" min="0" />
                        </div>
                    </div>
                    <div class="field full" v-if="form.strategy.cancelMode === 'alwaysReject'">
                        <label>撤单拒单原因码</label>
                        <input v-model.number="form.strategy.cancelRejectReason" type="number" min="0" />
                        <span class="hint">回填到撤单拒单/业务拒绝消息的原因字段</span>
                    </div>
                </div>
            </div>
            <div class="modal-foot">
                <button class="btn" @click="emit('close')">取消</button>
                <button class="btn primary" @click="submit">保存</button>
            </div>
        </div>

        <!-- 脚本手册弹窗（内嵌 docs/ 下的编写说明，Markdown 渲染） -->
        <LuaDocViewer v-if="docShow" @close="docShow = false" />
        <!-- 同步持仓数据弹窗（Oracle 查询 + 持仓表编辑保存） -->
        <PositionSyncDialog
            v-if="posShow"
            :backend="backend"
            @close="posShow = false"
            @notify="(t, e) => emit('notify', t, e)"
        />
    </div>
</template>
