<script setup lang="ts">
// App.vue —— 界面主组件（根组件），承载页面布局与全局交互。
//
// .vue 单文件组件由三段组成：
//   <script>   逻辑：状态与事件处理
//   <template> 结构：HTML + Vue 模板语法
//   <style>    外观：颜色、间距、字体等样式
// Vue 采用数据驱动渲染：ref()/reactive() 包裹的响应式数据变化后，
// 依赖它的模板自动更新，无需手动操作 DOM。
//
// 本文件承担的职责（从上到下）：
//   1. 自定义标题栏（无边框窗口的最小化/最大化/关闭按钮）
//   2. 自定义右键菜单（含输入框的剪切/复制/粘贴菜单）
//   3. 与后端通信：每秒拉一次全量快照 + 订阅日志事件
//   4. 网关/平台的增删改查、启停等操作
//   5. 各种弹窗（网关命名、平台编辑、删除确认、赞赏）和 toast 提示
// 平台卡片、平台编辑器、日志面板拆成了 components/ 下的三个子组件。
import { computed, onBeforeUnmount, onMounted, reactive, ref } from "vue";
import type { Backend } from "./backend";
import { createBackend } from "./backend";
import type { AppCfg, BackendEntry, GatewayCategory, GatewayConfig, GatewaySnapshot, LogEvent, PlatformConfig, Snapshot } from "./types";
import { defaultPlatform, emptyStats, GATEWAY_CATEGORIES, gatewayCategoryLabel } from "./types";
import { APP_VERSION, CHANGELOG } from "./changelog";
import { checkForUpdate, type UpdateInfo } from "./updater";
import PlatformCard from "./components/PlatformCard.vue";
import PlatformEditor from "./components/PlatformEditor.vue";
import LogPanel from "./components/LogPanel.vue";
import PacketViewer from "./components/PacketViewer.vue";
import OrderViewer from "./components/OrderViewer.vue";
import rewardQr from "./assets/reward-qr.jpg";

// ---------- 自定义标题栏（无边框窗口） ----------
// 窗口在 tauri.conf.json 里配置成了无边框（decorations: false），
// 系统自带的标题栏没有了，所以要自己画标题栏和三个窗口按钮。

/** 是否运行在 Tauri 桌面壳里（纯浏览器打开时没有这个内部对象） */
const isTauri = "__TAURI_INTERNALS__" in window;
/** 当前是否处于最大化状态（决定按钮显示“最大化”还是“还原”图标） */
const isMaximized = ref(false);

/** 拿到当前窗口对象。用动态 import 是为了纯浏览器环境下不加载 Tauri 模块 */
async function currentWindow() {
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    return getCurrentWindow();
}

async function winMinimize() {
    (await currentWindow()).minimize();
}

async function winToggleMaximize() {
    const w = await currentWindow();
    await w.toggleMaximize();
    isMaximized.value = await w.isMaximized();
}

async function winClose() {
    (await currentWindow()).close();
}

// ---------- 赞赏作者 ----------
/** 赞赏弹窗是否显示 */
const showReward = ref(false);

// ---------- 界面配色主题 ----------
// 原理见 style.css 开头的说明：换主题 = 给 <html> 换一个 data-theme 属性值。
// 选择结果存在浏览器本地的 localStorage 里，下次启动自动恢复。

/** 可选主题列表：id 对应 style.css 里的 data-theme 值，bg/accent 用于选择面板的色块预览。
 * 优雅白放第一位并作为默认主题（浅色更适合日常办公环境） */
const THEMES = [
    { id: "light", name: "优雅白", bg: "#f4f6fa", accent: "#3b82f6" },
    { id: "dark", name: "经典黑", bg: "#0e1117", accent: "#4f8cff" },
    { id: "blue", name: "现代蓝", bg: "#081c36", accent: "#38bdf8" },
    { id: "gray", name: "深空灰", bg: "#1a1c20", accent: "#2dd4bf" },
    { id: "purple", name: "极光紫", bg: "#0f0a1e", accent: "#c084fc" },
];

/** localStorage 的存储键名 */
const THEME_KEY = "simx-theme";

/** 当前主题 id；优先读上次保存的，没存过或存的值已失效则用默认的优雅白 */
const theme = ref(
    THEMES.some((t) => t.id === localStorage.getItem(THEME_KEY)) ? localStorage.getItem(THEME_KEY)! : "light"
);

/** 配色选择面板是否展开 */
const showThemePicker = ref(false);

/** 应用并保存主题（点选面板里的某一项时调用） */
function applyTheme(id: string) {
    theme.value = id;
    document.documentElement.dataset.theme = id;
    localStorage.setItem(THEME_KEY, id);
    showThemePicker.value = false;
}

// 脚本加载时立即应用一次（不等页面渲染完），避免启动瞬间先闪一下默认黑色
document.documentElement.dataset.theme = theme.value;

// ---------- 自定义右键菜单 ----------
// 桌面软件里网页默认的右键菜单（“刷新/查看源代码”等）会露馅，
// 所以全局拦截 contextmenu 事件，换成自己画的菜单。

/** 菜单里的一项 */
interface CtxMenuItem {
    /** 显示的文字 */
    label: string;
    /** 危险操作（如删除），显示为红色 */
    danger?: boolean;
    /** 置灰不可点 */
    disabled?: boolean;
    /** 这一项只是分隔线 */
    divider?: boolean;
    /** 点击后执行的动作 */
    action?: () => void;
}

/** 当前菜单的状态：是否显示、出现在哪、有哪些项 */
const ctxMenu = reactive({ show: false, x: 0, y: 0, items: [] as CtxMenuItem[] });

/** 在鼠标位置弹出菜单（各区域的右键处理函数都最终调用这里） */
function openCtxMenu(e: MouseEvent, items: CtxMenuItem[]) {
    e.preventDefault(); // 阻止浏览器默认右键菜单
    e.stopPropagation(); // 阻止事件继续往外层冒泡（否则外层的右键处理会再弹一次）
    // 估算菜单尺寸，避免超出窗口边界：
    // 宽按固定 190px 算，高按“每项 32px、分隔线 9px”累加，
    // 若鼠标位置放不下就往左/往上收，保证菜单完整可见
    const mw = 190;
    const mh = items.reduce((h, it) => h + (it.divider ? 9 : 32), 10);
    ctxMenu.x = Math.min(e.clientX, window.innerWidth - mw - 6);
    ctxMenu.y = Math.min(e.clientY, window.innerHeight - mh - 6);
    ctxMenu.items = items;
    ctxMenu.show = true;
}

function closeCtxMenu() {
    ctxMenu.show = false;
}

/** 点击某一菜单项：先关菜单再执行动作（分隔线和置灰项不响应） */
function runCtxItem(it: CtxMenuItem) {
    if (it.disabled || it.divider) return;
    closeCtxMenu();
    it.action?.();
}

/** 全局右键入口（onMounted 里挂到 document 上，页面任何地方右键都先经过这里） */
function onGlobalContextMenu(e: MouseEvent) {
    // 输入框内弹自定义编辑菜单，其余区域屏蔽网页默认菜单
    const t = e.target as HTMLElement;
    const input = t.closest("input, textarea") as HTMLInputElement | HTMLTextAreaElement | null;
    if (input) {
        inputCtxMenu(e, input);
        return;
    }
    e.preventDefault();
}

// ---- 输入框编辑菜单（剪切/复制/粘贴/全选） ----
// 屏蔽了默认右键菜单后，输入框里的“粘贴”等操作也没了，必须自己补一套。

/** 读取输入框当前的选中区间（start==end 表示只有光标、没有选中文字） */
function getInputSelection(el: HTMLInputElement | HTMLTextAreaElement) {
    // number 等类型不支持 selection API，访问会抛异常
    try {
        return { start: el.selectionStart, end: el.selectionEnd };
    } catch {
        return { start: null, end: null };
    }
}

/** 把文本插入/替换到输入框的光标位置（实现“粘贴”） */
function insertToInput(el: HTMLInputElement | HTMLTextAreaElement, text: string) {
    el.focus();
    try {
        const { start, end } = getInputSelection(el);
        el.setRangeText(text, start ?? el.value.length, end ?? el.value.length, "end");
    } catch {
        el.value = text; // 不支持选区的输入框（如 number）直接替换整体内容
    }
    el.dispatchEvent(new Event("input", { bubbles: true })); // 触发 v-model 更新
}

/** 构造输入框的右键菜单：根据“有没有选中文字、可不可编辑”决定各项是否置灰 */
function inputCtxMenu(e: MouseEvent, el: HTMLInputElement | HTMLTextAreaElement) {
    const { start, end } = getInputSelection(el);
    const hasSel = start !== null && end !== null && start !== end;
    const editable = !el.readOnly && !el.disabled;
    openCtxMenu(e, [
        {
            label: "剪切",
            disabled: !hasSel || !editable,
            action: () => {
                el.focus();
                document.execCommand("cut");
            },
        },
        {
            label: "复制",
            disabled: !hasSel,
            action: () => {
                el.focus();
                document.execCommand("copy");
            },
        },
        {
            label: "粘贴",
            disabled: !editable,
            action: async () => {
                try {
                    const text = await navigator.clipboard.readText();
                    if (text) insertToInput(el, text);
                } catch {
                    showToast("无法读取剪贴板，请使用 Ctrl+V", true);
                }
            },
        },
        { label: "", divider: true },
        {
            label: "全选",
            disabled: !el.value,
            action: () => {
                el.focus();
                el.select();
            },
        },
    ]);
}

/** 全局按键：按 Esc 关闭右键菜单 */
function onGlobalKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
        closeCtxMenu();
        showThemePicker.value = false;
    }
}

// ---------- 后端连接 ----------
// backend 是与引擎通信的唯一通道（见 backend.ts），onMounted 里创建；
// 界面后端设置弹窗里可切换多个后端（本地引擎 + 若干远程）。
// 它不用 ref 包，因为它本身不需要驱动界面更新，变化的是下面两个状态。
let backend: Backend | null = null;
/** "local"（内嵌引擎）或 "remote"（连远程服务器），显示在右上角徽章里 */
const backendMode = ref("local");
/** 远程模式下的连接状态；本地模式恒为 true */
const backendConnected = ref(true);

/** 后端设置弹窗：维护后端列表（本地引擎固定 + 远程可增删），点选即切换 */
const backendSettings = reactive({
    show: false,
    list: [] as BackendEntry[],
    currentId: "local",
    // “添加远程后端”表单：名称可选，地址 + 端口自动拼出 WebSocket 地址（端口留空用默认 9800）
    newName: "",
    newHost: "",
    newPort: "9800",
});

/** 更新服务器根地址（如 http://192.168.1.10/update）；留空表示不检查更新 */
const updateUrl = ref("");

// ---------- 关于与更新 ----------
/** 关于弹窗是否显示（顶栏“ⓘ 关于”按钮触发） */
const showAbout = ref(false);
/** 当前版本号：Tauri 环境取应用真实版本，纯浏览器回退到内置常量 */
const currentVersion = ref(APP_VERSION);
/** 检查更新弹窗：发现新版本时弹出，携带服务器上的版本信息 */
const updateBox = reactive({ show: false, info: null as UpdateInfo | null, busy: false, msg: "" });

/** 读取应用版本号（仅 Tauri 环境有真实版本，失败时保持内置回退值） */
async function loadVersion() {
    if (!isTauri) return;
    try {
        const { getVersion } = await import("@tauri-apps/api/app");
        currentVersion.value = await getVersion();
    } catch {
        // 读取失败不阻塞启动，保持内置版本号
    }
}

/** 执行一次更新检查：silent 为 true 时只有“发现新版”才弹窗（启动自动检查用） */
async function runUpdateCheck(silent: boolean) {
    const url = updateUrl.value.trim();
    if (!url) {
        if (!silent) showToast("未配置更新服务器地址，请在后端设置里填写", true);
        return;
    }
    const info = await checkForUpdate(url, currentVersion.value);
    if (info) {
        updateBox.info = info;
        updateBox.msg = "";
        updateBox.show = true;
    } else if (!silent) {
        showToast("当前已是最新版本");
    }
}

/** 下载新版本安装包并启动安装（由后端 command 执行，返回提示文案） */
async function downloadUpdate() {
    const info = updateBox.info;
    if (!info || !info.installerUrl || updateBox.busy) return;
    if (!isTauri) {
        // 纯浏览器环境没有后端可下载，直接打开下载地址交给浏览器
        window.open(info.installerUrl, "_blank");
        updateBox.show = false;
        return;
    }
    updateBox.busy = true;
    updateBox.msg = "正在下载安装包…";
    try {
        const { invoke } = await import("@tauri-apps/api/core");
        updateBox.msg = (await invoke("download_update", { url: info.installerUrl })) as string;
        showToast("已下载新版本，请按安装向导完成升级");
        updateBox.show = false;
    } catch (e) {
        updateBox.msg = `下载失败：${e}`;
    } finally {
        updateBox.busy = false;
    }
}

// ---------- 全局状态 ----------
// 界面采用最省心的“轮询快照”模式：每秒向后端要一次全部网关/平台/
// 连接/统计数据，整体替换 snapshot，Vue 自动把变化反映到页面上。
// 好处是逻辑极简、永远不会漏更新；代价是最多有 1 秒延迟，对本工具足够。

/** 全量快照：界面上所有网关/平台数据的唯一来源 */
const snapshot = ref<Snapshot>({ gateways: [] });
/** 侧栏当前选中的网关 id */
const selectedGwId = ref("");
/** 日志面板的内容（后端推送的事件累积在这里） */
const logs = ref<LogEvent[]>([]);
/** 日志最多保留 800 条，超出丢最旧的，防止长时间运行占用内存过多 */
const MAX_LOGS = 800;
/** 每秒轮询的定时器句柄（组件卸载时要清掉） */
let pollTimer: number | undefined;

// ---------- 弹窗状态 ----------
/** 网关命名弹窗：新建和重命名共用，靠 isNew 区分；category 仅新建时可改 */
const gwEditor = reactive({ show: false, isNew: true, id: "", name: "", category: "sz" as GatewayCategory });
/** 平台编辑弹窗：platform 存正在编辑的副本，保存时才写回后端 */
const pfEditor = reactive({
    show: false,
    isNew: true,
    platform: defaultPlatform(10001) as PlatformConfig,
});
/** 收发报文弹窗：点击平台卡片上的“报文”按钮时打开（平台级聚合） */
const pvViewer = reactive({
    show: false,
    gatewayId: "",
    platformId: "",
    platformName: "",
});
/** 订单弹窗：点击平台卡片上的“订单”按钮时打开 */
const ovViewer = reactive({
    show: false,
    gatewayId: "",
    platformId: "",
    platformName: "",
    listenAddr: "",
});
/** 通用确认框：先把“确认后要做的事”存进 action，用户点确定才执行 */
const confirmBox = reactive({ show: false, text: "", action: null as null | (() => void) });

// ---------- 提示 ----------
/** 屏幕下方的消息气泡（操作成功/失败都用它提示，自动消失） */
const toast = reactive({ show: false, text: "", isError: false });
let toastTimer: number | undefined;

/** 弹一条提示：错误显示 4.2 秒（多给点时间看清），普通消息 2.2 秒 */
function showToast(text: string, isError = false) {
    toast.text = text;
    toast.isError = isError;
    toast.show = true;
    clearTimeout(toastTimer); // 连续弹多条时重新计时，避免新消息被旧定时器提前关掉
    toastTimer = window.setTimeout(() => (toast.show = false), isError ? 4200 : 2200);
}

// ---------- 数据 ----------
// computed（计算属性）：从已有数据推导出的“派生数据”，
// 依赖的数据一变它自动重算，不需要手动同步。

/** 当前选中的网关快照（没选或已被删除时为 null） */
const selectedGw = computed<GatewaySnapshot | null>(
    () => snapshot.value.gateways.find((g) => g.config.id === selectedGwId.value) ?? null,
);

/** 顶栏徽章文案：本地引擎固定显示；远程显示名称 + 连接状态 */
const badgeText = computed(() => {
    if (backendMode.value === "local") return "本地引擎";
    const name = backendSettings.list.find((b) => b.id === backendSettings.currentId)?.name;
    const label = name && name !== "远程后端" ? name : "远程后端";
    return backendConnected.value ? `${label}·已连接` : `${label}·连接中…`;
});

/** 顶栏的全局统计：把所有网关下所有平台的计数加总 */
const globalStats = computed(() => {
    const total = emptyStats();
    let conns = 0;
    for (const gw of snapshot.value.gateways) {
        for (const p of gw.platforms) {
            total.orders += p.stats.orders;
            total.acks += p.stats.acks;
            total.trades += p.stats.trades;
            // 两种拒单（执行报告拒绝 + 业务拒绝）合并成一个数展示
            total.orderRejects += p.stats.orderRejects + p.stats.businessRejects;
            conns += p.connections.length;
        }
    }
    return { ...total, conns };
});

/** 在网关快照里找某个平台的运行状态（传给 PlatformCard 显示） */
function platformSnapshotOf(gw: GatewaySnapshot, platformId: string) {
    return gw.platforms.find((p) => p.platformId === platformId);
}

// ---------- 后端调用 ----------
/** 统一的命令发送入口：失败自动弹错误 toast，成功返回数据。
 *  各操作函数都走这里，错误提示就不用每处重写一遍 */
async function call(payload: Record<string, unknown>): Promise<unknown | null> {
    if (!backend) return null;
    const resp = await backend.dispatch(payload);
    if (!resp.ok) {
        showToast(resp.error ?? "操作失败", true);
        return null;
    }
    return resp.data ?? null;
}

/** 拉一次全量快照刷新界面（每秒定时调用，各操作完成后也会主动调一次） */
async function refresh() {
    if (!backend || !backend.isConnected()) return;
    const resp = await backend.dispatch({ cmd: "get_snapshot" });
    if (resp.ok && resp.data) {
        snapshot.value = resp.data as Snapshot;
        // 首次拿到数据时自动选中第一个网关，免得打开后主区空白
        if (!selectedGwId.value && snapshot.value.gateways.length) {
            selectedGwId.value = snapshot.value.gateways[0].config.id;
        }
    }
}

// ---------- 后端切换 ----------
// “后端连接设置”弹窗里的保存流程：写配置文件 → 断开旧连接 → 按新配置重建 → 重新订阅。

/** 订阅后端的状态变化与日志事件（每次创建 backend 后调用一次） */
function wireBackend() {
    if (!backend) return;
    backend.onStatusChange((c) => {
        backendConnected.value = c;
        if (c) refresh(); // 断线重连成功后立刻刷新一次，不等下一秒轮询
    });
    backend.onEvent((ev) => {
        // 目前后端只推 log 一种事件；超过上限就从头部删掉最旧的
        if (ev.event === "log") {
            logs.value.push(ev as unknown as LogEvent);
            if (logs.value.length > MAX_LOGS) {
                logs.value.splice(0, logs.value.length - MAX_LOGS);
            }
        }
    });
}

/** 打开后端设置弹窗（顶栏徽章点击触发） */
function openBackendCfg() {
    backendSettings.show = true;
}

/** 把配置归一化成“列表 + 当前选中”：本地引擎恒在首位；旧配置只有单地址时合成一条 */
function normalizeBackends(cfg: AppCfg): { list: BackendEntry[]; currentId: string } {
    const list: BackendEntry[] = [{ id: "local", name: "本地引擎", kind: "local", url: "" }];
    const seen = new Set(["local"]);
    for (const b of cfg.backends ?? []) {
        if (!seen.has(b.id) && b.kind !== "local") {
            seen.add(b.id);
            list.push(b);
        }
    }
    // 旧版配置文件只有 backend/remoteUrl 两个字段：把“远程”合成一个条目
    if (cfg.backends?.length === 0 && cfg.backend === "remote" && cfg.remoteUrl) {
        list.push({ id: "remote", name: "远程后端", kind: "remote", url: cfg.remoteUrl });
    }
    let currentId = "local";
    if (cfg.backend && list.some((b) => b.id === cfg.backend)) {
        currentId = cfg.backend;
    } else if (cfg.backend === "remote" && list.some((b) => b.id === "remote")) {
        currentId = "remote";
    }
    return { list, currentId };
}

/** 把后端列表与当前选中写回持久层：Tauri 下写 simx.config.json，纯浏览器下存 localStorage */
async function persistAppCfg() {
    const cfg: AppCfg = {
        backend: backendSettings.currentId,
        remoteUrl:
            backendSettings.list.find((b) => b.id === backendSettings.currentId)?.url ?? "",
        backends: backendSettings.list,
        updateUrl: updateUrl.value,
    };
    if (isTauri) {
        const { invoke } = await import("@tauri-apps/api/core");
        await invoke("set_app_config", { config: cfg });
    } else {
        localStorage.setItem("simx.backends", JSON.stringify(backendSettings.list));
        localStorage.setItem("simx.backend", backendSettings.currentId);
        localStorage.setItem("simx.updateUrl", updateUrl.value);
    }
}

/** 保存更新服务器地址（后端设置弹窗里修改后立即生效） */
async function saveUpdateUrl() {
    try {
        await persistAppCfg();
        showToast("更新服务器地址已保存");
    } catch (e) {
        showToast(`保存失败：${e}`, true);
    }
}

/** 切换到指定后端：写配置 → 断开旧连接 → 按条目重建 → 重新订阅 → 清空旧数据等新快照 */
async function applyBackend(id: string) {
    const entry = backendSettings.list.find((b) => b.id === id);
    if (!entry) return;
    backendSettings.currentId = id;
    try {
        await persistAppCfg();
    } catch (e) {
        showToast(`保存配置失败：${e}`, true);
        return;
    }
    backend?.dispose();
    backend = await createBackend(entry);
    backendMode.value = backend.mode;
    backendConnected.value = backend.isConnected();
    wireBackend();
    // 后端换了，界面上的旧数据作废：清空后等新后端的第一份快照
    snapshot.value = { gateways: [] };
    selectedGwId.value = "";
    await refresh();
    showToast(entry.kind === "local" ? "已切换到本地引擎" : `已切换到「${entry.name}」`);
}

/** 添加远程后端表单的实时预览：把地址/端口拼成完整 WebSocket 地址，
 * 支持三种输入：① 纯地址（自动补 ws:// 与 :9800/ws）② 地址带端口 ③ 直接粘贴完整 ws:// 地址 */
const addUrlPreview = computed(() => {
    const host = backendSettings.newHost.trim();
    if (!host) return "";
    if (/^wss?:\/\//.test(host)) {
        // 已带协议：只补 /ws 路径（若用户忘了写）
        return host.endsWith("/ws") ? host : host.replace(/\/+$/, "") + "/ws";
    }
    const port = backendSettings.newPort.trim() || "9800";
    // 地址里已带冒号（如 192.168.1.10:9810）视为已含端口，不再拼
    return `ws://${host.includes(":") ? host : `${host}:${port}`}/ws`;
});

/** 添加远程后端并立即切换过去 */
async function addBackend() {
    const name = backendSettings.newName.trim();
    const url = addUrlPreview.value;
    if (!url) {
        showToast("请输入服务器地址（IP 或域名，可带端口）", true);
        return;
    }
    const entry: BackendEntry = {
        id: `b-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`,
        // 名称留空时按地址自动生成，避免出现空名条目
        name: name || `远程·${backendSettings.newHost.trim()}`,
        kind: "remote",
        url,
    };
    backendSettings.list.push(entry);
    backendSettings.newName = "";
    backendSettings.newHost = "";
    backendSettings.newPort = "9800";
    backendSettings.show = false;
    await applyBackend(entry.id);
}

/** 删除远程后端；删的是当前选中项时自动切回本地引擎（本地引擎不可删） */
async function removeBackend(id: string) {
    const idx = backendSettings.list.findIndex((b) => b.id === id);
    if (idx < 0 || backendSettings.list[idx].kind === "local") return;
    backendSettings.list.splice(idx, 1);
    if (backendSettings.currentId === id) {
        await applyBackend("local");
    } else {
        try {
            await persistAppCfg();
        } catch (e) {
            showToast(`保存配置失败：${e}`, true);
        }
    }
}

// ---------- 网关操作 ----------
// 后端只有一个 save_gateway 命令：新建/改名/增删平台都是“把整个
// 网关配置提交上去覆盖保存”，所以下面几个保存函数都是先拼好完整配置再发。

/** 打开“新建网关”弹窗，预填一个默认名 */
function openNewGateway() {
    gwEditor.isNew = true;
    gwEditor.id = "";
    gwEditor.name = `网关 ${snapshot.value.gateways.length + 1}`;
    gwEditor.category = "sz";
    gwEditor.show = true;
}

/** 打开“重命名网关”弹窗，预填当前名字（分类只读） */
function openRenameGateway() {
    if (!selectedGw.value) return;
    gwEditor.isNew = false;
    gwEditor.id = selectedGw.value.config.id;
    gwEditor.name = selectedGw.value.config.name;
    gwEditor.category = selectedGw.value.config.category ?? "sz";
    gwEditor.show = true;
}

/** 保存网关名称（新建和重命名共用） */
async function saveGatewayName() {
    const name = gwEditor.name.trim() || "未命名网关";
    let gw: GatewayConfig;
    if (gwEditor.isNew) {
        // id 留空，后端会自动分配一个新 id；分类新建时选定后不可改
        gw = { id: "", name, category: gwEditor.category, platforms: [] };
    } else {
        const cur = snapshot.value.gateways.find((g) => g.config.id === gwEditor.id);
        if (!cur) return;
        // JSON 一转一解是“深拷贝”的简易写法：复制出完全独立的副本，
        // 改副本不会影响界面上正在显示的原数据（分类保持不变）
        gw = { ...JSON.parse(JSON.stringify(cur.config)), name };
    }
    const saved = (await call({ cmd: "save_gateway", gateway: gw })) as GatewayConfig | null;
    if (saved) {
        gwEditor.show = false;
        selectedGwId.value = saved.id;
        await refresh();
        showToast(gwEditor.isNew ? "网关已创建" : "网关已保存");
    }
}

/** 删除网关前先弹确认框（真正的删除动作存进 confirmBox.action 等用户确认） */
function askDeleteGateway() {
    if (!selectedGw.value) return;
    const id = selectedGw.value.config.id;
    const name = selectedGw.value.config.name;
    confirmBox.text = `确定删除网关「${name}」及其全部平台配置？`;
    confirmBox.action = async () => {
        // call 失败时已弹错误提示并返回 null，此时保持选中、不刷新
        const res = await call({ cmd: "delete_gateway", id });
        if (res !== null) {
            selectedGwId.value = "";
            await refresh();
        }
    };
    confirmBox.show = true;
}

/** 启动/停止网关（侧栏右键和主区按钮共用） */
async function toggleGateway(gw: GatewaySnapshot) {
    const cmd = gw.running ? "stop_gateway" : "start_gateway";
    const r = await backend?.dispatch({ cmd, id: gw.config.id });
    if (r && !r.ok) {
        showToast(r.error ?? "操作失败", true);
    } else {
        showToast(gw.running ? "网关已停止" : "网关已启动");
    }
    await refresh();
}

/** 把当前网关下所有平台的统计计数清零 */
async function resetStats() {
    if (!selectedGw.value) return;
    await call({ cmd: "reset_stats", id: selectedGw.value.config.id });
    await refresh();
    showToast("统计已重置");
}

// ---------- 平台操作 ----------
/** 新建平台时自动挑一个没被占用的端口：从 10001 往上找第一个空位 */
function nextFreePort(): number {
    const used = new Set<number>();
    snapshot.value.gateways.forEach((g) => g.config.platforms.forEach((p) => used.add(p.port)));
    let port = 10001;
    while (used.has(port)) port += 1;
    return port;
}

/** 打开“新建平台”编辑器，用默认配置 + 空闲端口预填（按网关分类） */
function openNewPlatform() {
    if (!selectedGw.value) return;
    pfEditor.isNew = true;
    pfEditor.platform = defaultPlatform(nextFreePort(), selectedGw.value.config.category ?? "sz");
    pfEditor.show = true;
}

/** 打开“编辑平台”：深拷贝一份给编辑器改，点保存前不碰原数据 */
function openEditPlatform(p: PlatformConfig) {
    pfEditor.isNew = false;
    pfEditor.platform = JSON.parse(JSON.stringify(p));
    pfEditor.show = true;
}

/** 保存平台（编辑器点“保存”时回调）：把平台塞进所属网关配置里整体提交。
 *  网关运行中只允许改“模拟回报策略与回报延迟”（后端实时生效），
 *  此时走 update_strategy 热更新命令，其余字段需停止网关后再保存 */
async function savePlatform(p: PlatformConfig) {
    if (!selectedGw.value) return;
    const gw: GatewayConfig = JSON.parse(JSON.stringify(selectedGw.value.config));
    if (selectedGw.value.running && !pfEditor.isNew) {
        const r = await backend?.dispatch({
            cmd: "update_strategy",
            gatewayId: gw.id,
            platformId: p.id,
            strategy: p.strategy,
        });
        if (r && !r.ok) {
            showToast(r.error ?? "策略更新失败", true);
            return;
        }
        pfEditor.show = false;
        await refresh();
        showToast("策略与回报延迟已实时生效（其余修改需停止网关后保存）");
        return;
    }
    if (pfEditor.isNew) {
        gw.platforms.push(p);
    } else {
        // 按 id 找到原来那个平台替换掉；找不到（罕见）则当新增处理
        const idx = gw.platforms.findIndex((x) => x.id === p.id);
        if (idx >= 0) gw.platforms[idx] = p;
        else gw.platforms.push(p);
    }
    const saved = await call({ cmd: "save_gateway", gateway: gw });
    if (saved) {
        pfEditor.show = false;
        await refresh();
        showToast(pfEditor.isNew ? "平台已添加" : "平台已保存");
    }
}

/** 点击平台卡片上的“报文”按钮：打开该平台的收发报文弹窗（平台级聚合，
 *  报文按平台而非连接展示，连接号在弹窗内每行标注）。
 *  只把定位信息传给弹窗组件，报文由组件自己向后端轮询拉取 */
function openPacketViewer(p: PlatformConfig) {
    if (!selectedGw.value) return;
    pvViewer.gatewayId = selectedGw.value.config.id;
    pvViewer.platformId = p.id;
    pvViewer.platformName = p.name;
    pvViewer.show = true;
}

/** 点击平台卡片上的“订单”按钮：打开该平台的订单列表弹窗。
 *  只把定位信息传给弹窗组件，订单由组件自己向后端轮询拉取 */
function openOrderViewer(p: PlatformConfig) {
    if (!selectedGw.value) return;
    ovViewer.gatewayId = selectedGw.value.config.id;
    ovViewer.platformId = p.id;
    ovViewer.platformName = p.name;
    ovViewer.listenAddr = `${p.listenHost}:${p.port}`;
    ovViewer.show = true;
}

/** 删除平台前先弹确认框：确认后把该平台从网关配置里剔除再整体保存 */
function askRemovePlatform(p: PlatformConfig) {
    if (!selectedGw.value) return;
    const gwId = selectedGw.value.config.id;
    confirmBox.text = `确定删除平台「${p.name}」？`;
    confirmBox.action = async () => {
        const cur = snapshot.value.gateways.find((g) => g.config.id === gwId);
        if (!cur) return;
        const gw: GatewayConfig = JSON.parse(JSON.stringify(cur.config));
        gw.platforms = gw.platforms.filter((x) => x.id !== p.id);
        if (await call({ cmd: "save_gateway", gateway: gw })) {
            await refresh();
        }
    };
    confirmBox.show = true;
}

/** 确认框点“删除”：执行之前存好的动作，然后清空 */
function runConfirm() {
    confirmBox.show = false;
    confirmBox.action?.();
    confirmBox.action = null;
}

// ---------- 各区域右键菜单 ----------
// 每个区域（侧栏网关项/平台卡片/空白处/日志面板）根据自己的
// 上下文拼一份菜单项列表，交给 openCtxMenu 弹出。

/** 侧栏网关项的右键菜单（右键时顺便选中该网关，菜单动作才能作用到它） */
function gwCtxMenu(e: MouseEvent, gw: GatewaySnapshot) {
    selectedGwId.value = gw.config.id;
    openCtxMenu(e, [
        { label: gw.running ? "■ 停止网关" : "▶ 启动网关", action: () => toggleGateway(gw) },
        // 运行中不允许改结构（加/删平台、删网关），避免监听中的端口状态错乱
        { label: "＋ 添加平台", disabled: gw.running, action: openNewPlatform },
        { label: "重命名网关", action: openRenameGateway },
        { label: "重置统计", action: resetStats },
        { label: "", divider: true },
        { label: "删除网关", danger: true, disabled: gw.running, action: askDeleteGateway },
    ]);
}

/** 平台卡片的右键菜单 */
function platformCtxMenu(e: MouseEvent, p: PlatformConfig) {
    const running = selectedGw.value?.running ?? false;
    openCtxMenu(e, [
        // 运行中也可编辑：保存时只热更新“模拟回报策略与回报延迟”，
        // 其余字段（端口等）由后端拦截并提示需停止网关后修改
        { label: "编辑平台", action: () => openEditPlatform(p) },
        {
            label: "复制监听地址",
            action: () => {
                navigator.clipboard.writeText(`${p.listenHost}:${p.port}`);
                showToast("已复制监听地址");
            },
        },
        { label: "", divider: true },
        { label: "删除平台", danger: true, disabled: running, action: () => askRemovePlatform(p) },
    ]);
}

/** 空白区域（侧栏空处/主区背景）的右键菜单 */
function blankCtxMenu(e: MouseEvent) {
    const items: CtxMenuItem[] = [{ label: "＋ 新建网关", action: openNewGateway }];
    if (selectedGw.value) {
        items.push({ label: "＋ 添加平台", disabled: selectedGw.value.running, action: openNewPlatform });
    }
    items.push({ label: "刷新", action: () => void refresh() });
    openCtxMenu(e, items);
}

/** 日志面板的右键菜单（复制选中文字/清空日志） */
function logCtxMenu(e: MouseEvent) {
    const sel = window.getSelection()?.toString() ?? "";
    openCtxMenu(e, [
        {
            label: "复制选中内容",
            disabled: !sel,
            action: () => {
                navigator.clipboard.writeText(sel);
                showToast("已复制");
            },
        },
        { label: "", divider: true },
        { label: "清空日志", danger: true, disabled: !logs.value.length, action: () => (logs.value = []) },
    ]);
}

// ---------- 生命周期 ----------
// onMounted：页面刚显示出来时执行一次，相当于“开机自检 + 接线”：
// 读配置 → 建后端连接 → 订阅状态/日志 → 拉首次快照 → 开启每秒轮询 → 挂全局监听。
onMounted(async () => {
    // 读配置并归一化成后端列表；非 Tauri（纯浏览器）环境读 localStorage
    let appCfg: AppCfg = { backend: "local", remoteUrl: "", backends: [], updateUrl: "" };
    if (isTauri) {
        try {
            const { invoke } = await import("@tauri-apps/api/core");
            appCfg = (await invoke("get_app_config")) as AppCfg;
        } catch {
            // 读取失败按默认（本地模式）处理
        }
    } else {
        try {
            const list = JSON.parse(
                localStorage.getItem("simx.backends") ?? "[]",
            ) as BackendEntry[];
            const currentId = localStorage.getItem("simx.backend") ?? "";
            appCfg = { backend: currentId, remoteUrl: "", backends: list, updateUrl: "" };
        } catch {
            // 配置损坏按默认处理
        }
    }
    const norm = normalizeBackends(appCfg);
    backendSettings.list = norm.list;
    backendSettings.currentId = norm.currentId;
    // 更新服务器地址：浏览器环境单独存 localStorage，Tauri 环境在 simx.config.json 里
    updateUrl.value = isTauri
        ? appCfg.updateUrl ?? ""
        : localStorage.getItem("simx.updateUrl") ?? "";
    backend = await createBackend(norm.list.find((b) => b.id === norm.currentId) ?? norm.list[0]);
    backendMode.value = backend.mode;
    backendConnected.value = backend.isConnected();
    wireBackend();
    await refresh();
    pollTimer = window.setInterval(refresh, 1000);
    document.addEventListener("contextmenu", onGlobalContextMenu);
    document.addEventListener("keydown", onGlobalKeydown);
    await loadVersion();
    // 启动 3 秒后静默检查一次更新：不打扰使用，只有“发现新版”才弹窗
    setTimeout(() => void runUpdateCheck(true), 3000);
});

// 页面即将卸载时把定时器和全局监听都清掉，避免“人走了闹钟还在响”
onBeforeUnmount(() => {
    clearInterval(pollTimer);
    backend?.dispose(); // 断掉后端连接，停掉远程模式的重连定时器
    document.removeEventListener("contextmenu", onGlobalContextMenu);
    document.removeEventListener("keydown", onGlobalKeydown);
});
</script>

<template>
    <!--
        【模板语法速查】看下面 HTML 时认识这几个记号就够了：
        {{ xxx }}      把数据显示到页面上
        v-if / v-for   按条件显示 / 按列表循环生成元素
        :xxx="..."     把属性绑到数据上（数据变属性跟着变）
        @click="..."   监听事件（点击/右键等），触发 script 里的函数
        v-model="xxx"  输入框与数据双向绑定（改哪边另一边都跟着变）
        data-tauri-drag-region —— Tauri 的约定：按住这个元素可拖动窗口
    -->
    <div class="app">
        <!-- 顶栏（兼作自定义标题栏，空白处可拖动窗口） -->
        <header class="topbar" data-tauri-drag-region>
            <div class="brand" data-tauri-drag-region>
                <span class="brand-mark" data-tauri-drag-region>◆</span>
                <span class="brand-name" data-tauri-drag-region>SimuXchange</span>
                <span class="brand-sub" data-tauri-drag-region>模拟撮合网关</span>
            </div>
            <div class="topbar-stats" data-tauri-drag-region>
                <div class="tstat" data-tauri-drag-region><span class="tstat-v num">{{ globalStats.conns }}</span><span class="tstat-l">连接</span></div>
                <div class="tstat" data-tauri-drag-region><span class="tstat-v num">{{ globalStats.orders }}</span><span class="tstat-l">委托</span></div>
                <div class="tstat" data-tauri-drag-region><span class="tstat-v num c-blue">{{ globalStats.acks }}</span><span class="tstat-l">确认</span></div>
                <div class="tstat" data-tauri-drag-region><span class="tstat-v num c-green">{{ globalStats.trades }}</span><span class="tstat-l">成交</span></div>
                <div class="tstat" data-tauri-drag-region><span class="tstat-v num c-red">{{ globalStats.orderRejects }}</span><span class="tstat-l">拒单</span></div>
            </div>
            <div class="topbar-right">
                <button class="badge" :class="backendConnected ? 'green' : 'red'" title="点击切换后端" @click="openBackendCfg">
                    <span class="dot" :class="backendConnected ? 'on' : 'off'"></span>
                    {{ badgeText }}
                </button>
                <button class="reward-btn" title="赞赏作者" @click="showReward = true">♥ 赞赏</button>
                <button class="about-btn" title="版本信息与更新" @click="showAbout = true">ⓘ 关于</button>
                <!-- 配色主题按钮：点击展开/收起选择面板 -->
                <button class="win-btn theme-btn" title="界面配色" @click="showThemePicker = !showThemePicker">◐</button>
                <div v-if="isTauri" class="win-controls">
                    <button class="win-btn" title="最小化" @click="winMinimize">─</button>
                    <button class="win-btn" :title="isMaximized ? '还原' : '最大化'" @click="winToggleMaximize">
                        {{ isMaximized ? "❐" : "□" }}
                    </button>
                    <button class="win-btn close" title="关闭" @click="winClose">✕</button>
                </div>
            </div>
        </header>

        <div class="layout">
            <!-- 侧栏：网关列表 -->
            <aside class="sidebar" @contextmenu="blankCtxMenu">
                <div class="side-head">
                    <span>交易网关</span>
                    <button class="btn sm primary" @click="openNewGateway">＋ 新建</button>
                </div>
                <div class="gw-list">
                    <div
                        v-for="gw in snapshot.gateways"
                        :key="gw.config.id"
                        class="gw-item"
                        :class="{ active: gw.config.id === selectedGwId }"
                        @click="selectedGwId = gw.config.id"
                        @contextmenu="gwCtxMenu($event, gw)"
                    >
                        <span class="dot" :class="gw.running ? 'on pulse' : 'off'"></span>
                        <div class="gw-item-main">
                            <div class="gw-item-name">{{ gw.config.name }}</div>
                            <div class="gw-item-sub">{{ gatewayCategoryLabel(gw.config.category ?? "sz") }} · {{ gw.config.platforms.length }} 个平台</div>
                        </div>
                        <span class="badge" :class="gw.running ? 'green' : 'gray'">
                            {{ gw.running ? "运行" : "停止" }}
                        </span>
                    </div>
                    <div v-if="!snapshot.gateways.length" class="gw-empty">
                        暂无网关<br />点击「新建」创建第一个交易网关
                    </div>
                </div>
            </aside>

            <!-- 主区 -->
            <main class="main" @contextmenu="blankCtxMenu">
                <template v-if="selectedGw">
                    <div class="gw-head">
                        <div class="gw-head-left">
                            <h2>{{ selectedGw.config.name }}</h2>
                            <span class="badge blue">{{ gatewayCategoryLabel(selectedGw.config.category ?? "sz") }}</span>
                            <span class="badge" :class="selectedGw.running ? 'green' : 'gray'">
                                <span class="dot" :class="selectedGw.running ? 'on pulse' : 'off'"></span>
                                {{ selectedGw.running ? "运行中" : "已停止" }}
                            </span>
                        </div>
                        <div class="gw-head-actions">
                            <button
                                class="btn"
                                :class="selectedGw.running ? 'danger-ghost' : 'success'"
                                @click="toggleGateway(selectedGw)"
                            >
                                {{ selectedGw.running ? "■ 停止网关" : "▶ 启动网关" }}
                            </button>
                            <button class="btn" :disabled="selectedGw.running" @click="openNewPlatform">＋ 添加平台</button>
                            <button class="btn ghost" @click="resetStats">重置统计</button>
                            <button class="btn ghost" :disabled="selectedGw.running" @click="openRenameGateway">重命名</button>
                            <button class="btn ghost danger-ghost" :disabled="selectedGw.running" @click="askDeleteGateway">删除</button>
                        </div>
                    </div>

                    <div class="platform-grid" v-if="selectedGw.config.platforms.length">
                        <PlatformCard
                            v-for="p in selectedGw.config.platforms"
                            :key="p.id"
                            :platform="p"
                            :category="selectedGw.config.category ?? 'sz'"
                            :snapshot="platformSnapshotOf(selectedGw, p.id)"
                            :gateway-running="selectedGw.running"
                            @edit="openEditPlatform(p)"
                            @remove="askRemovePlatform(p)"
                            @view-packets="openPacketViewer(p)"
                            @view-orders="openOrderViewer(p)"
                            @contextmenu="platformCtxMenu($event, p)"
                        />
                    </div>
                    <div v-else class="main-empty">
                        <div class="empty-icon">▦</div>
                        <p>该网关下还没有平台</p>
                        <button class="btn primary" :disabled="selectedGw.running" @click="openNewPlatform">＋ 添加平台</button>
                    </div>
                </template>
                <div v-else class="main-empty">
                    <div class="empty-icon">◆</div>
                    <p>新建或选择左侧的交易网关开始</p>
                    <button class="btn primary" @click="openNewGateway">＋ 新建网关</button>
                </div>
            </main>
        </div>

        <!-- 日志面板 -->
        <LogPanel :logs="logs" @clear="logs = []" @contextmenu="logCtxMenu" />

        <!-- 网关名称弹窗 -->
        <div v-if="gwEditor.show" class="modal-mask" @mousedown.self="gwEditor.show = false">
            <div class="modal" style="width: 380px">
                <div class="modal-head">
                    <span>{{ gwEditor.isNew ? "新建网关" : "重命名网关" }}</span>
                    <button class="modal-close" @click="gwEditor.show = false">✕</button>
                </div>
                <div class="modal-body">
                    <div class="field">
                        <label>网关名称</label>
                        <input v-model="gwEditor.name" @keyup.enter="saveGatewayName" autofocus />
                    </div>
                    <div class="field">
                        <label>网关分类<span v-if="!gwEditor.isNew" class="hint">（创建后不可修改）</span></label>
                        <select v-model="gwEditor.category" :disabled="!gwEditor.isNew">
                            <option v-for="c in GATEWAY_CATEGORIES" :key="c.value" :value="c.value">
                                {{ c.label }}
                            </option>
                        </select>
                    </div>
                </div>
                <div class="modal-foot">
                    <button class="btn" @click="gwEditor.show = false">取消</button>
                    <button class="btn primary" @click="saveGatewayName">保存</button>
                </div>
            </div>
        </div>

        <!-- 平台编辑弹窗 -->
        <PlatformEditor
            v-if="pfEditor.show"
            :platform="pfEditor.platform"
            :category="selectedGw?.config.category ?? 'sz'"
            :is-new="pfEditor.isNew"
            @save="savePlatform"
            @close="pfEditor.show = false"
        />

        <!-- 收发报文弹窗（点击平台卡片上的“报文”按钮时打开） -->
        <PacketViewer
            v-if="pvViewer.show"
            :backend="backend"
            :gateway-id="pvViewer.gatewayId"
            :platform-id="pvViewer.platformId"
            :platform-name="pvViewer.platformName"
            @close="pvViewer.show = false"
        />

        <!-- 订单弹窗（点击平台卡片上的“订单”按钮时打开） -->
        <OrderViewer
            v-if="ovViewer.show"
            :backend="backend"
            :gateway-id="ovViewer.gatewayId"
            :platform-id="ovViewer.platformId"
            :platform-name="ovViewer.platformName"
            :listen-addr="ovViewer.listenAddr"
            :category="selectedGw?.config.category ?? 'sz'"
            @close="ovViewer.show = false"
            @notify="(t, e) => showToast(t, e)"
        />

        <!-- 后端设置弹窗：管理多个后端（本地引擎 + 远程），点选即切换 -->
        <div v-if="backendSettings.show" class="modal-mask" @mousedown.self="backendSettings.show = false">
            <div class="modal" style="width: 500px">
                <div class="modal-head">
                    <span>后端连接设置</span>
                    <button class="modal-close" @click="backendSettings.show = false">✕</button>
                </div>
                <div class="modal-body">
                    <label class="be-sec-label">已配置的后端（点击切换）</label>
                    <div class="be-list">
                        <div
                            v-for="b in backendSettings.list"
                            :key="b.id"
                            class="be-item"
                            :class="{ active: b.id === backendSettings.currentId }"
                            @click="applyBackend(b.id)"
                        >
                            <span class="be-radio"></span>
                            <span class="be-name">{{ b.name }}</span>
                            <span class="be-url">{{ b.kind === "local" ? "界面内嵌引擎" : b.url }}</span>
                            <button
                                v-if="b.kind !== 'local'"
                                class="be-del"
                                title="删除该后端"
                                @click.stop="removeBackend(b.id)"
                            >
                                ✕
                            </button>
                        </div>
                    </div>
                    <label class="be-sec-label">添加远程后端（只需地址，可带端口）</label>
                    <div class="be-add">
                        <input v-model="backendSettings.newName" placeholder="名称（可选）" @keyup.enter="addBackend" />
                        <input v-model="backendSettings.newHost" placeholder="服务器地址，如 192.168.1.10" @keyup.enter="addBackend" />
                        <input v-model="backendSettings.newPort" placeholder="端口" @keyup.enter="addBackend" />
                    </div>
                    <p class="hint" style="margin-bottom: 8px">
                        {{ addUrlPreview ? `将连接：${addUrlPreview}` : "地址留空时自动补 ws:// 前缀与 /ws 路径；地址里带端口（如 192.168.1.10:9810）则无需再填端口" }}
                    </p>
                    <p class="hint">
                        本地引擎为固定项（窗口关闭即停止）；远程后端运行在服务器上，
                        切换立即生效并写入 simx.config.json，下次启动保持。
                    </p>
                    <label class="be-sec-label">更新服务器地址（可选）</label>
                    <input
                        v-model="updateUrl"
                        placeholder="如 http://192.168.1.10/update（留空不检查更新）"
                        @change="saveUpdateUrl"
                        @keyup.enter="saveUpdateUrl"
                    />
                    <p class="hint">
                        启动时自动检查该服务器上的 version.json；发现新版本会弹窗提示下载安装包。
                        服务器上需放置 version.json 与安装包，见《产品使用手册》“自动升级”章节。
                    </p>
                </div>
                <div class="modal-foot">
                    <button class="btn" @click="backendSettings.show = false">关闭</button>
                    <button class="btn primary" @click="addBackend">添加并切换</button>
                </div>
            </div>
        </div>

        <!-- 确认弹窗 -->
        <div v-if="confirmBox.show" class="modal-mask" @mousedown.self="confirmBox.show = false">
            <div class="modal" style="width: 360px">
                <div class="modal-head"><span>确认操作</span></div>
                <div class="modal-body">
                    <p style="color: var(--text-dim); line-height: 1.7">{{ confirmBox.text }}</p>
                </div>
                <div class="modal-foot">
                    <button class="btn" @click="confirmBox.show = false">取消</button>
                    <button class="btn primary" style="background: var(--red); border-color: var(--red)" @click="runConfirm">
                        删除
                    </button>
                </div>
            </div>
        </div>

        <!-- 赞赏作者弹窗 -->
        <div v-if="showReward" class="modal-mask" @mousedown.self="showReward = false">
            <div class="modal" style="width: 340px">
                <div class="modal-head">
                    <span>♥ 赞赏作者</span>
                    <button class="modal-close" @click="showReward = false">✕</button>
                </div>
                <div class="modal-body reward-body">
                    <p class="reward-text">如果这个项目对你有帮助，欢迎请作者喝杯咖啡 ☕</p>
                    <img :src="rewardQr" class="reward-qr" alt="支付宝赞赏二维码" />
                    <p class="reward-tip">支付宝扫码赞赏</p>
                    <p class="reward-contact">
                        联系邮箱：<a href="mailto:chenzhe1986@126.com">chenzhe1986@126.com</a>
                    </p>
                    <p class="reward-license">本项目遵循 GPL 开源协议</p>
                </div>
            </div>
        </div>

        <!-- 关于弹窗：版本号 + 更新说明 + 手动检查更新 -->
        <div v-if="showAbout" class="modal-mask" @mousedown.self="showAbout = false">
            <div class="modal" style="width: 380px">
                <div class="modal-head">
                    <span>关于 SimuXchange</span>
                    <button class="modal-close" @click="showAbout = false">✕</button>
                </div>
                <div class="modal-body about-body">
                    <div class="about-logo">◆</div>
                    <div class="about-name">SimuXchange · 模拟撮合网关</div>
                    <div class="about-ver">版本 v{{ currentVersion }}</div>
                    <div class="about-sec">更新说明</div>
                    <ul class="about-notes">
                        <li v-for="(n, i) in CHANGELOG[0]?.notes ?? []" :key="i">{{ n }}</li>
                    </ul>
                    <p class="about-license">本项目遵循 GPL 开源协议</p>
                </div>
                <div class="modal-foot">
                    <button class="btn ghost" @click="showAbout = false">关闭</button>
                    <button class="btn primary" @click="runUpdateCheck(false)">检查更新</button>
                </div>
            </div>
        </div>

        <!-- 发现新版本弹窗：版本号 + 更新说明 + 下载安装 -->
        <div v-if="updateBox.show" class="modal-mask" @mousedown.self="updateBox.show = false">
            <div class="modal" style="width: 420px">
                <div class="modal-head">
                    <span>发现新版本 v{{ updateBox.info?.manifest.version }}</span>
                    <button class="modal-close" @click="updateBox.show = false">✕</button>
                </div>
                <div class="modal-body">
                    <p class="hint" style="margin-bottom: 8px">当前版本 v{{ currentVersion }}</p>
                    <ul class="about-notes" v-if="updateBox.info?.manifest.notes?.length">
                        <li v-for="(n, i) in updateBox.info?.manifest.notes" :key="i">{{ n }}</li>
                    </ul>
                    <p v-if="updateBox.msg" class="update-msg">{{ updateBox.msg }}</p>
                </div>
                <div class="modal-foot">
                    <button class="btn" :disabled="updateBox.busy" @click="updateBox.show = false">
                        以后再说
                    </button>
                    <button class="btn primary" :disabled="updateBox.busy || !updateBox.info?.installerUrl" @click="downloadUpdate">
                        {{ updateBox.busy ? "下载中…" : "下载并安装" }}
                    </button>
                </div>
            </div>
        </div>

        <!-- 配色主题选择面板（透明遮罩实现"点外面就关闭"，与右键菜单同一套思路） -->
        <div v-if="showThemePicker" class="ctx-mask" @mousedown="showThemePicker = false" @contextmenu.prevent="showThemePicker = false">
            <div class="theme-pop" @mousedown.stop>
                <div class="theme-pop-title">界面配色</div>
                <div
                    v-for="t in THEMES"
                    :key="t.id"
                    class="theme-item"
                    :class="{ active: theme === t.id }"
                    @click="applyTheme(t.id)"
                >
                    <!-- 色块预览：外圈是主题背景色，中间小点是点缀色 -->
                    <span class="theme-swatch" :style="{ background: t.bg }">
                        <span class="theme-swatch-dot" :style="{ background: t.accent }"></span>
                    </span>
                    <span class="theme-name">{{ t.name }}</span>
                    <span v-if="theme === t.id" class="theme-check">✓</span>
                </div>
            </div>
        </div>

        <!-- 自定义右键菜单 -->
        <div v-if="ctxMenu.show" class="ctx-mask" @mousedown="closeCtxMenu" @contextmenu.prevent="closeCtxMenu">
            <div class="ctx-menu" :style="{ left: ctxMenu.x + 'px', top: ctxMenu.y + 'px' }" @mousedown.stop>
                <template v-for="(it, i) in ctxMenu.items" :key="i">
                    <div v-if="it.divider" class="ctx-divider"></div>
                    <div
                        v-else
                        class="ctx-item"
                        :class="{ danger: it.danger, disabled: it.disabled }"
                        @click="runCtxItem(it)"
                    >
                        {{ it.label }}
                    </div>
                </template>
            </div>
        </div>

        <!-- Toast -->
        <transition name="toast">
            <div v-if="toast.show" class="toast" :class="{ error: toast.isError }">{{ toast.text }}</div>
        </transition>
    </div>
</template>

<style scoped>
.app {
    height: 100%;
    display: flex;
    flex-direction: column;
}

/* 顶栏（兼作标题栏） */
.topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 20px;
    padding: 8px 8px 8px 18px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-soft);
    flex-shrink: 0;
    user-select: none;
}

.brand {
    display: flex;
    align-items: baseline;
    gap: 8px;
}

.brand-mark {
    color: var(--accent);
    font-size: 15px;
}

.brand-name {
    font-size: 15px;
    font-weight: 700;
    letter-spacing: 0.3px;
}

.brand-sub {
    font-size: 11.5px;
    color: var(--text-faint);
}

.topbar-stats {
    display: flex;
    gap: 26px;
}

.tstat {
    display: flex;
    align-items: baseline;
    gap: 6px;
}

.tstat-v {
    font-size: 16px;
    font-weight: 600;
}

.tstat-l {
    font-size: 11px;
    color: var(--text-faint);
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

/* 顶栏右侧：后端状态 / 赞赏 / 窗口控制 */
.topbar-right {
    display: flex;
    align-items: center;
    gap: 10px;
}

.reward-btn {
    border: 1px solid var(--border);
    background: transparent;
    color: #f472b6;
    font-size: 11.5px;
    padding: 4px 10px;
    border-radius: 30px;
    cursor: pointer;
    transition: all 0.15s;
}

.reward-btn:hover {
    border-color: #f472b6;
    background: rgba(244, 114, 182, 0.1);
}

/* 顶栏“关于”按钮（与赞赏按钮同形，用主题色区分） */
.about-btn {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--accent);
    font-size: 11.5px;
    padding: 4px 10px;
    border-radius: 30px;
    cursor: pointer;
    transition: all 0.15s;
}

.about-btn:hover {
    border-color: var(--accent);
    background: var(--accent-soft);
}

/* 关于弹窗 */
.about-body {
    text-align: center;
}

.about-logo {
    font-size: 30px;
    color: var(--accent);
    line-height: 1.4;
}

.about-name {
    font-size: 14.5px;
    font-weight: 650;
    margin-top: 4px;
}

.about-ver {
    font-size: 12px;
    color: var(--text-dim);
    margin-top: 2px;
}

.about-sec {
    text-align: left;
    font-size: 11.5px;
    color: var(--text-faint);
    margin-top: 14px;
    margin-bottom: 4px;
}

.about-notes {
    text-align: left;
    list-style: disc;
    padding-left: 18px;
    font-size: 12.5px;
    color: var(--text-dim);
    line-height: 1.9;
}

.about-license {
    font-size: 11px;
    color: var(--text-faint);
    margin-top: 12px;
    padding-top: 10px;
    border-top: 1px dashed var(--border);
}

/* 更新弹窗里的下载状态提示 */
.update-msg {
    font-size: 12px;
    color: var(--accent);
    margin-top: 10px;
}

.win-controls {
    display: flex;
    margin-left: 4px;
}

.win-btn {
    width: 34px;
    height: 30px;
    border: none;
    background: transparent;
    color: var(--text-dim);
    font-size: 12px;
    cursor: pointer;
    border-radius: var(--radius-sm);
    transition: background 0.12s;
}

.win-btn:hover {
    background: var(--panel-2);
    color: var(--text);
}

.win-btn.close:hover {
    background: var(--red);
    color: #fff;
}

/* 配色主题选择面板（固定在顶栏配色按钮下方） */
.theme-pop {
    position: fixed;
    top: 44px;
    right: 96px;
    width: 168px;
    background: var(--panel-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    box-shadow: var(--shadow-pop);
    padding: 6px;
    user-select: none;
}

.theme-pop-title {
    font-size: 11px;
    color: var(--text-faint);
    padding: 4px 9px 6px;
}

.theme-item {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 6px 9px;
    border-radius: 5px;
    cursor: pointer;
    transition: background 0.1s;
}

.theme-item:hover {
    background: var(--accent-soft);
}

.theme-item.active .theme-name {
    color: var(--accent);
    font-weight: 600;
}

/* 色块预览：外圈主题背景色 + 中心点缀色小点，一眼看出主题气质 */
.theme-swatch {
    width: 18px;
    height: 18px;
    border-radius: 50%;
    border: 1px solid var(--border);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
}

.theme-swatch-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
}

.theme-name {
    flex: 1;
    font-size: 12.5px;
    color: var(--text);
}

.theme-check {
    color: var(--accent);
    font-size: 12px;
}

/* 赞赏弹窗 */
.reward-body {
    text-align: center;
}

.reward-text {
    font-size: 12.5px;
    color: var(--text-dim);
    line-height: 1.7;
    margin-bottom: 12px;
}

.reward-qr {
    width: 220px;
    max-width: 100%;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    display: block;
    margin: 0 auto;
}

.reward-tip {
    font-size: 11.5px;
    color: var(--text-faint);
    margin-top: 10px;
}

.reward-contact {
    font-size: 12px;
    color: var(--text-dim);
    margin-top: 12px;
}

.reward-contact a {
    color: var(--accent);
    text-decoration: none;
}

.reward-contact a:hover {
    text-decoration: underline;
}

.reward-license {
    font-size: 11px;
    color: var(--text-faint);
    margin-top: 8px;
    padding-top: 10px;
    border-top: 1px dashed var(--border);
}

/* 布局 */
.layout {
    flex: 1;
    display: flex;
    min-height: 0;
}

.sidebar {
    width: 230px;
    flex-shrink: 0;
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    background: var(--bg-soft);
}

.side-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 14px;
    font-size: 12.5px;
    font-weight: 600;
    color: var(--text-dim);
}

.gw-list {
    flex: 1;
    overflow-y: auto;
    padding: 0 8px 10px;
    display: flex;
    flex-direction: column;
    gap: 4px;
}

.gw-item {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 9px 10px;
    border-radius: var(--radius-sm);
    cursor: pointer;
    border: 1px solid transparent;
    transition: background 0.12s;
}

.gw-item:hover {
    background: var(--panel);
}

.gw-item.active {
    background: var(--panel-2);
    border-color: var(--border);
}

.gw-item-main {
    flex: 1;
    min-width: 0;
}

.gw-item-name {
    font-size: 13px;
    font-weight: 500;
    /* 长网关名允许换行完整显示（如“DEV分组7-深圳模拟撮合1”），
       不再用省略号截断；anywhere 允许在任意字符处断行 */
    white-space: normal;
    overflow-wrap: anywhere;
    line-height: 1.35;
}

.gw-item-sub {
    font-size: 11px;
    color: var(--text-faint);
    margin-top: 1px;
}

.gw-empty {
    padding: 30px 10px;
    text-align: center;
    color: var(--text-faint);
    font-size: 12px;
    line-height: 2;
}

/* 主区 */
.main {
    flex: 1;
    overflow-y: auto;
    padding: 16px 20px;
    min-width: 0;
}

.gw-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 10px;
    margin-bottom: 16px;
}

.gw-head-left {
    display: flex;
    align-items: center;
    gap: 12px;
}

.gw-head-left h2 {
    font-size: 18px;
    font-weight: 650;
}

.gw-head-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
}

.platform-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(360px, 1fr));
    gap: 14px;
}

.main-empty {
    height: 70%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 14px;
    color: var(--text-faint);
}

.empty-icon {
    font-size: 34px;
    opacity: 0.4;
}

/* 自定义右键菜单 */
.ctx-mask {
    position: fixed;
    inset: 0;
    z-index: 300;
}

.ctx-menu {
    position: absolute;
    min-width: 170px;
    background: var(--panel-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    box-shadow: var(--shadow-pop);
    padding: 5px;
    user-select: none;
}

.ctx-item {
    padding: 7px 12px;
    font-size: 12.5px;
    color: var(--text);
    border-radius: 4px;
    cursor: pointer;
    white-space: nowrap;
    transition: background 0.1s;
}

.ctx-item:hover {
    background: var(--accent);
    color: #fff;
}

.ctx-item.danger {
    color: var(--red);
}

.ctx-item.danger:hover {
    background: var(--red);
    color: #fff;
}

.ctx-item.disabled {
    opacity: 0.38;
    cursor: default;
}

.ctx-item.disabled:hover {
    background: transparent;
    color: var(--text);
}

.ctx-item.danger.disabled:hover {
    color: var(--red);
}

.ctx-divider {
    height: 1px;
    background: var(--border);
    margin: 4px 8px;
}

/* Toast */
.toast {
    position: fixed;
    bottom: 224px;
    left: 50%;
    transform: translateX(-50%);
    background: var(--panel-2);
    border: 1px solid var(--border);
    color: var(--text);
    padding: 9px 20px;
    border-radius: 30px;
    font-size: 12.5px;
    box-shadow: var(--shadow-pop);
    z-index: 200;
}

.toast.error {
    border-color: rgba(248, 113, 113, 0.5);
    color: var(--red);
}

.toast-enter-active,
.toast-leave-active {
    transition: all 0.22s;
}

.toast-enter-from,
.toast-leave-to {
    opacity: 0;
    transform: translateX(-50%) translateY(8px);
}
</style>
