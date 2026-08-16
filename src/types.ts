// 与后端（simx-core）JSON 结构对应的类型定义
//
// 注意：interface 是 TypeScript 编译期类型（类比 C++ 纯类型声明），
// 不产生运行时开销，编辑器据此做静态检查。每个类型与后端 Rust 结构体
// 一一对应（后端序列化时已把 snake_case 转为 camelCase），修改后端
// 结构时需同步更新本文件。

/** 应用配置（来自 exe 同目录的 simx.config.json） */
export interface AppCfg {
    /** 当前选中的后端 id（"local" 或 backends 里某个远程项的 id） */
    backend: string;
    /** 兼容旧字段：远程模式的 WebSocket 地址（列表为空时的回退值） */
    remoteUrl: string;
    /** 已配置的后端列表（含固定项“本地引擎”），界面可随时切换 */
    backends: BackendEntry[];
    /** 更新服务器根地址（启动时自动检查新版本；留空则不检查） */
    updateUrl: string;
}

/** 一个已配置的后端：本地引擎（固定）或远程 simx-server */
export interface BackendEntry {
    /** 唯一标识："local" 固定代表本地引擎，远程项由前端生成 */
    id: string;
    /** 显示名称（界面后端列表里展示） */
    name: string;
    /** 类型：local（内嵌引擎）/ remote（远程 simx-server） */
    kind: "local" | "remote" | string;
    /** 远程地址（kind 为 remote 时有效） */
    url: string;
}

/** 回报延迟：两值都为 0 = 同步；相等 = 固定延迟；否则在区间内随机 */
export interface DelayConfig {
    minMs: number;
    maxMs: number;
}

/** 七种回报策略（与后端 strategy.rs 的 StrategyMode 对应） */
export type StrategyMode =
    | "fullSingle"
    | "fullSplit"
    | "partialSingle"
    | "partialSplit"
    | "noAutoReply"
    | "ackOnly"
    | "reject";

/** 拒单方式：执行回报(200102) 或 业务拒绝消息(MsgType=4) */
export type RejectVia = "executionReport" | "businessReject";

/** 回报策略配置（平台编辑器里能改的都在这） */
export interface StrategyConfig {
    mode: StrategyMode;
    /** 拆单笔数随机区间（仅拆单策略用） */
    splitCountMin: number;
    splitCountMax: number;
    /** 价格阶梯档位（元），拆单成交的逐笔价差 */
    priceTick: number;
    rejectVia: RejectVia;
    /** 拒单原因代码（回填到报文的 OrdRejReason 字段） */
    rejectReason: number;
    /** 拒单文字说明（仅业务拒绝方式携带） */
    rejectText: string;
    /** 确认回报延迟 */
    ackDelay: DelayConfig;
    /** 成交回报延迟（拆单时每笔单独抽样） */
    tradeDelay: DelayConfig;
}

/** 一个模拟交易平台（= 一个 TCP 监听端口）的配置 */
export interface PlatformConfig {
    id: string;
    name: string;
    /** 平台类型编号（见下方 PLATFORM_TYPES） */
    platformType: number;
    /** 监听地址，0.0.0.0 = 所有网卡 */
    listenHost: string;
    port: number;
    /** 本网关在 Logon 回复中的发送方代码 */
    compId: string;
    /** 是否校验登录密码 */
    checkPassword: boolean;
    password: string;
    /** 平台分区号（兼容单分区旧字段；partitionNos 为空时生效） */
    partitionNo: number;
    /** 分区号列表（逗号分隔，如 "101,102,103,104"）：
     *  回报按证券代码哈希分配到其中一个分区（同一证券恒落同一分区） */
    partitionNos: string;
    /** 是否展示该平台各连接的收发报文（勾选即自动持久化到文件） */
    showPackets: boolean;
    /** 是否把收发报文持久化到文件（已与展示合并：随展示开关联动，
     *  保留字段仅为兼容旧配置/旧后端） */
    persistPackets: boolean;
    /** 是否缓存收到的订单（开启后可查看订单列表、撤单按真实状态回复） */
    cacheOrders: boolean;
    strategy: StrategyConfig;
}

/** 网关分类（与后端 config.rs 的 GatewayCategory 对应）：
 *  sz = 深圳统一网关（深交所 Binary 协议）；
 *  shjj = 上海竞价网关（上交所竞价平台 Binary 0.54 协议）；
 *  shbond = 上海新债券网关（上交所新债券平台 Binary 1.90 协议） */
export type GatewayCategory = "sz" | "shjj" | "shbond";

/** 一个网关 = 若干个平台的集合，整体启停 */
export interface GatewayConfig {
    id: string;
    name: string;
    /** 网关分类，决定网关下所有平台说哪套协议 */
    category: GatewayCategory;
    platforms: PlatformConfig[];
    /** 上次退出时是否在运行（后端 --auto-start 恢复依据，随配置存 gateways.json） */
    wasRunning: boolean;
}

/** 平台统计计数（后端 stats.rs 的快照） */
export interface StatsSnapshot {
    orders: number;
    acks: number;
    trades: number;
    orderRejects: number;
    businessRejects: number;
    cancels: number;
    totalConnections: number;
}

/** 一个已连接的柜台 */
export interface ConnInfo {
    id: number;
    /** 对方 IP:端口 */
    peer: string;
    /** 柜台 Logon 时报的机构代码 */
    compId: string;
    /** 是否已完成登录 */
    loggedOn: boolean;
    /** 连接建立时间 HH:MM:SS */
    since: string;
}

/** 平台运行状态快照 */
export interface PlatformSnapshot {
    platformId: string;
    listening: boolean;
    stats: StatsSnapshot;
    connections: ConnInfo[];
}

/** 网关快照：配置 + 是否运行 + 各平台状态 */
export interface GatewaySnapshot {
    config: GatewayConfig;
    running: boolean;
    platforms: PlatformSnapshot[];
}

/** 全量快照（前端每秒拉一次，拿到后整体替换界面状态） */
export interface Snapshot {
    gateways: GatewaySnapshot[];
}

/** 后端推送的日志事件（显示在底部日志面板） */
export interface LogEvent {
    event: "log";
    level: "info" | "warn" | "error";
    message: string;
    ts: string;
    gatewayId: string;
    platformId: string;
}

/** 报文解析出的一个字段（交易所规范字段名 + 可读值） */
export interface ParsedField {
    /** 交易所规范字段名，如 ClOrdID、SecurityID、Side */
    name: string;
    /** 可读值：价格/数量换算成自然单位、时间戳转可读时间、枚举附中文含义 */
    value: string;
}

/** 一条收发报文记录（16 进制展示 + 颜色区分方向 + 按字段解析结果） */
export interface PacketRecord {
    /** 平台内序号（跨连接全局唯一，增量拉取游标） */
    seq: number;
    /** 所属连接序号（平台级弹窗里标注是哪条连接的报文） */
    connId: number;
    /** 时间戳 HH:MM:SS.mmm */
    ts: string;
    /** recv = 收到柜台报文；send = 发给柜台报文 */
    dir: "recv" | "send";
    /** 原始字节的大写十六进制（空格分隔） */
    hex: string;
    /** 按交易所字段名解析出的字段列表（空 = 未解析成功，只展示原始报文） */
    fields?: ParsedField[];
}

/** 报文弹窗轮询拉取到的一批报文 */
export interface PacketPage {
    packets: PacketRecord[];
    /** 当前最大 seq（把游标推进到这里） */
    latestSeq: number;
    /** 该连接是否持久化（true 时徽标显示“完整历史”） */
    persist: boolean;
    /** 连接摘要列表（平台弹窗按连接分组展示的分组标题）；旧后端可能没有，兜底空数组 */
    conns?: PacketConn[];
}

/** 平台报文弹窗里一条连接的摘要（分组标题的信息来源） */
export interface PacketConn {
    /** 连接序号（与 PacketRecord.connId 对应） */
    connId: number;
    /** 对端地址（柜台 IP:端口） */
    peer: string;
    /** 接入时间 HH:MM:SS */
    since: string;
    /** 是否仍在线（断开的连接保留历史报文，标记为已离线） */
    alive: boolean;
}

// ---- 订单缓存（与后端 orderbook.rs 对应） ----

/** 订单状态（后端 OrderStatus 枚举的 camelCase 序列化） */
export type OrderStatus = "new" | "partial" | "filled" | "cancelled" | "rejected";

/** 一条缓存的订单（数量/价格都是自然单位：股/元，直接展示） */
export interface OrderEntry {
    /** 委托编号（撤单请求的 OrigClOrdID 指向它） */
    clOrdId: string;
    /** 交易所分配的订单号（收到确认回报后回填，此前为空） */
    orderId: string;
    /** 证券代码 */
    securityId: string;
    /** 方向中文（"买"/"卖"） */
    side: string;
    /** 委托价（元） */
    price: number;
    /** 委托量（股） */
    qty: number;
    /** 累计成交量（股） */
    cumQty: number;
    /** 剩余量（股） */
    leavesQty: number;
    /** 订单状态 */
    status: OrderStatus;
    /** 委托类型（协议原值） */
    ordType: number;
    /** 证券账户 */
    account: string;
    /** 营业部代码 */
    branch: string;
    /** 收到委托的时间 HH:MM:SS */
    ts: string;
    /** 所属连接序号（订单来自哪条连接，后端手动回复时定位发送通道） */
    connId: number;
    /** 回报交易单元：沪市为 Pbu（登录 CompID 前 8 位）、深市为申报交易单元 */
    pbu: string;
    /** 业务标识（沪市 BizID 回填用；深市无此概念，恒为 0） */
    bizId: number;
    /** 业务 PBU（沪市 BizPbu 回填用；深市无此概念，恒为空） */
    bizPbu: string;
    /** 订单所有者类型（回报回填用；深市为 u16，沪市为 u8） */
    ownerType: number;
    /** 信用标签（沪市回填用；深市无，恒为空） */
    creditTag: string;
    /** 结算会员代码（回报回填用） */
    clearingFirm: string;
    /** 用户私有信息（回报按规范回填上行值） */
    userInfo: string;
    /** 业务标识字符串：深市为委托 ApplID（如 "010" 现货竞价）；沪市无，恒为空串 */
    biz: string;
}

/** 订单状态 → 中文标签与颜色（未知值兜底为“未知”） */
export function orderStatusOf(s: OrderStatus): { label: string; cls: string } {
    switch (s) {
        case "new":
            return { label: "已报", cls: "blue" };
        case "partial":
            return { label: "部分成交", cls: "amber" };
        case "filled":
            return { label: "全部成交", cls: "green" };
        case "cancelled":
            return { label: "已撤", cls: "gray" };
        case "rejected":
            return { label: "已拒绝", cls: "red" };
        default:
            return { label: String(s), cls: "gray" };
    }
}

/** 是否在途（已报/部分成交）：在途订单收到撤单请求会撤单成功 */
export function isInflight(s: OrderStatus): boolean {
    return s === "new" || s === "partial";
}

// ---- 展示辅助：给界面用的选项列表、标签文本、默认值 ----

/** 深交所平台类型编号对照表（来自接口规范文档） */
export const PLATFORM_TYPES: { value: number; label: string }[] = [
    { value: 1, label: "现货集中竞价交易平台" },
    { value: 2, label: "综合金融服务平台" },
    { value: 3, label: "非交易处理平台" },
    { value: 4, label: "衍生品集中竞价交易平台" },
    { value: 5, label: "国际市场互联平台" },
    { value: 6, label: "固定收益交易平台" },
];

/** 上交所竞价网关平台类型对照表（竞价平台 PlatformID=0） */
export const PLATFORM_TYPES_SHJJ: { value: number; label: string }[] = [
    { value: 0, label: "竞价平台" },
];

/** 上交所新债券网关平台类型对照表（新债券平台 PlatformID=2） */
export const PLATFORM_TYPES_SHBOND: { value: number; label: string }[] = [
    { value: 2, label: "新债券平台" },
];

/** 网关分类下拉框选项（value 必须与后端枚举的 camelCase 名一致） */
export const GATEWAY_CATEGORIES: { value: GatewayCategory; label: string }[] = [
    { value: "sz", label: "深圳统一网关" },
    { value: "shjj", label: "上海竞价网关" },
    { value: "shbond", label: "上海新债券网关" },
];

/** 按网关分类返回对应的平台类型对照表 */
export function platformTypesOf(category: GatewayCategory): { value: number; label: string }[] {
    if (category === "shjj") return PLATFORM_TYPES_SHJJ;
    if (category === "shbond") return PLATFORM_TYPES_SHBOND;
    return PLATFORM_TYPES;
}

/** 策略下拉框选项（value 必须与后端枚举的 camelCase 名一致） */
export const STRATEGY_MODES: { value: StrategyMode; label: string }[] = [
    { value: "fullSingle", label: "全部成交（单笔）" },
    { value: "fullSplit", label: "全部成交（多笔拆单）" },
    { value: "partialSingle", label: "部分成交（单笔）" },
    { value: "partialSplit", label: "部分成交（多笔拆单）" },
    { value: "noAutoReply", label: "不自动回复（挂单手动回复）" },
    { value: "ackOnly", label: "不成交挂单（只回确认）" },
    { value: "reject", label: "拒单" },
];

/** 策略枚举值 → 中文标签（未知值原样返回） */
export function strategyModeLabel(mode: StrategyMode): string {
    return STRATEGY_MODES.find((m) => m.value === mode)?.label ?? mode;
}

/** 网关分类 → 中文标签（未知值原样返回） */
export function gatewayCategoryLabel(c: GatewayCategory): string {
    return GATEWAY_CATEGORIES.find((g) => g.value === c)?.label ?? c;
}

/** 平台类型编号 → 中文标签（按网关分类选对照表；未知值兜底） */
export function platformTypeLabel(t: number, category: GatewayCategory = "sz"): string {
    return platformTypesOf(category).find((p) => p.value === t)?.label ?? `平台${t}`;
}

/** 延迟配置 → 显示文本（同步 / 固定值 / 区间） */
export function delayLabel(d: DelayConfig): string {
    if (!d || (d.minMs === 0 && d.maxMs === 0)) return "同步";
    if (d.minMs === d.maxMs) return `${d.minMs}ms`;
    return `${d.minMs}~${d.maxMs}ms`;
}

/** 拒单原因代码默认值：深圳 20009（价格错误-超涨跌幅限制），上海 1025（价格错误） */
export function defaultRejectReason(category: GatewayCategory): number {
    return category === "sz" ? 20009 : 1025;
}

/** 新建平台时的默认策略：全部成交（单笔）、无延迟。
 *  拒单参数按网关分类给默认值（深圳 20009 / 上海 1025） */
export function defaultStrategy(category: GatewayCategory = "sz"): StrategyConfig {
    return {
        mode: "fullSingle",
        splitCountMin: 2,
        splitCountMax: 5,
        priceTick: 0.01,
        rejectVia: "executionReport",
        rejectReason: defaultRejectReason(category),
        rejectText: "模拟拒单",
        ackDelay: { minMs: 0, maxMs: 0 },
        tradeDelay: { minMs: 0, maxMs: 0 },
    };
}

/** 分区号列表默认值：现货竞价（深市平台类型 1）与上海竞价/新债券
 *  默认 "101,102,103,104"（多分区按证券哈希分配），其它平台类型默认 "166" */
export function defaultPartitionNos(platformType: number, category: GatewayCategory = "sz"): string {
    if (category === "shjj" || category === "shbond") return "101,102,103,104";
    if (category === "sz" && platformType === 1) return "101,102,103,104";
    return "166";
}

/** 新建平台的默认配置（id 留空由后端生成；端口由调用方指定）。
 *  按网关分类给出不同默认值：上海竞价平台 PlatformID=0、无需密码校验。 */
export function defaultPlatform(port: number, category: GatewayCategory = "sz"): PlatformConfig {
    if (category === "shjj") {
        return {
            id: "",
            name: "竞价平台",
            platformType: 0,
            listenHost: "0.0.0.0",
            port,
            compId: "SIMX_TGW",
            checkPassword: false,
            password: "",
            partitionNo: 101,
            partitionNos: defaultPartitionNos(0, category),
            showPackets: true,
            persistPackets: true,
            cacheOrders: true,
            strategy: defaultStrategy(category),
        };
    }
    if (category === "shbond") {
        return {
            id: "",
            name: "新债券平台",
            platformType: 2,
            listenHost: "0.0.0.0",
            port,
            compId: "SIMX_TGW",
            checkPassword: false,
            password: "",
            partitionNo: 101,
            partitionNos: defaultPartitionNos(2, category),
            showPackets: true,
            persistPackets: true,
            cacheOrders: true,
            strategy: defaultStrategy(category),
        };
    }
    return {
        id: "",
        name: "现货集中竞价交易平台",
        platformType: 1,
        listenHost: "0.0.0.0",
        port,
        compId: "SIMX_TGW",
        checkPassword: false,
        password: "",
        partitionNo: 101,
        partitionNos: defaultPartitionNos(1, category),
        showPackets: true,
        persistPackets: true,
        cacheOrders: true,
        strategy: defaultStrategy(category),
    };
}

/** 全零统计（未运行平台的占位数据） */
export function emptyStats(): StatsSnapshot {
    return {
        orders: 0,
        acks: 0,
        trades: 0,
        orderRejects: 0,
        businessRejects: 0,
        cancels: 0,
        totalConnections: 0,
    };
}
