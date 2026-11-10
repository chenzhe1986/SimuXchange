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
    /** “AI 生成脚本”的大模型接口配置；浏览器模式读不到配置文件时无此字段
     *  （Rust 侧缺省时始终返回默认值，纯浏览器由 llm.ts 回退内置默认） */
    llm?: LlmCfg;
}

/** “AI 生成脚本”的大模型接口配置（与 simx-core 的 LlmConfig 对应，
 *  来自 simx.config.json 的 llm 字段，OpenAI 兼容的 chat/completions 协议） */
export interface LlmCfg {
    /** 接口根地址（写到版本号为止，前端自动拼 /chat/completions） */
    baseUrl: string;
    /** 服务商鉴权密钥（请求头 Authorization: Bearer <apiKey>） */
    apiKey: string;
    /** 模型 id */
    model: string;
    /** 采样温度（脚本生成要求稳定，取低值） */
    temperature: number;
    /** 单次输出的 token 上限 */
    maxTokens: number;
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

/** 报单自动回报模式（与后端 strategy.rs 的 StrategyMode 对应） */
export type StrategyMode =
    | "fullSingle"
    | "fullSplit"
    | "partialSingle"
    | "partialSplit"
    | "noAutoReply"
    | "ackOnly"
    | "reject"
    | "luaScript";

/** 拒单方式：执行回报(200102) 或 业务拒绝消息(MsgType=4) */
export type RejectVia = "executionReport" | "businessReject";

/** 撤单自动回报模式（与后端 config.rs 的 CancelMode 对应） */
export type CancelMode = "default" | "alwaysSuccess" | "alwaysReject";

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
    /** 撤单自动回报模式 */
    cancelMode: CancelMode;
    /** 撤单拒单原因代码（回填到撤单拒单/业务拒绝消息） */
    cancelRejectReason: number;
    /** 前台拒单：撤单拒单时改发业务拒绝消息 */
    cancelFrontReject: boolean;
    /** 撤单回报延迟（撤单成功与撤单拒单回报） */
    cancelDelay: DelayConfig;
    /** Lua 回报脚本源码（仅 luaScript 模式使用；保存时后端校验语法，
     *  运行时沙箱化执行，详见 docs/Lua脚本回报编写说明.md） */
    luaScript: string;
    /** ETF 清单文件目录（深圳非交易处理平台 / 上海综合平台）：收到 ETF 申赎委托时
     *  从该目录按证券代码查找最新的清单文件（深：pcf_<代码>_<YYYYMMDD>.xml；
     *  沪：ssepcf_<代码>_<YYYYMMDD>.xml），生成执行报告的成份券/现金替代明细；
     *  留空 = 不启用 */
    etfPcfDir: string;
}

/** 上海综合网关的 SQL Server 连接与接口表配置（与后端 config.rs 的 DbConfig 对应）。
 *  综合业务平台不走 TCP，柜台与网关通过 reqresp（委托表）/ execreport（成交表）交换 STEP 消息 */
export interface DbConfig {
    /** SQL Server 主机名或 IP（命名实例请填实例实际监听端口） */
    host: string;
    /** SQL Server 端口，默认 1433 */
    port: number;
    /** 数据库名 */
    database: string;
    /** 登录账号（SQL Server 身份验证） */
    user: string;
    /** 登录密码 */
    password: string;
    /** 委托表（请求及响应接口表）名称，默认 reqresp */
    reqRespTable: string;
    /** 成交表（执行报告表）名称，默认 execreport */
    execReportTable: string;
    /** 轮询间隔（毫秒） */
    pollIntervalMs: number;
}

/** Oracle 连接配置（“同步持仓数据”弹窗；与后端 positions.rs 的
 *  OracleConnectConfig 对应）。仅用于从持仓库查询，不参与网关通信 */
export interface OracleConnectConfig {
    /** 环境名（仅界面标识，如 "qa环境"） */
    envName: string;
    /** EZConnect 连接串，如 "10.210.50.63:2151/spdtrderbak" */
    connectString: string;
    /** 登录用户名，如 QA_SPEEDTRADER */
    user: string;
    /** 登录密码（只存服务端进程内存，不落盘） */
    password: string;
}

/** 一条 ETF 成份股持仓（查询 SQL 的六列；数量单位：股） */
export interface PositionRow {
    /** 证券代码 */
    securityId: string;
    /** 证券名称 */
    securityName: string;
    /** 持有数量 */
    holdingQty: number;
    /** 可用数量 */
    availableQty: number;
    /** 可申赎数量（ETF 申购扣减只认这个字段） */
    creationRedemptionQty: number;
    /** 冻结数量 */
    frozenQty: number;
}

/** 服务端持仓快照（弹窗打开时拉取）：synced=false 表示尚未保存过持仓，
 *  ETF 申购不按持仓扣减；rows 为内存中当前生效的唯一一份持仓 */
export interface PositionSnapshot {
    oracle: OracleConnectConfig;
    accountId: string;
    synced: boolean;
    rows: PositionRow[];
}

/** 一个模拟交易平台（= 一个 TCP 监听端口）的配置 */
export interface PlatformConfig {
    id: string;
    name: string;
    /** 平台类型编号（见下方 PLATFORM_TYPES） */
    platformType: number;
    /** 监听地址，0.0.0.0 = 所有网卡（上海综合网关不用） */
    listenHost: string;
    port: number;
    /** 上海综合网关的数据库配置（仅 category === "shzh" 时存在） */
    db?: DbConfig;
    /** 本网关在 Logon 回复中的发送方代码 */
    compId: string;
    /** 是否校验登录密码 */
    checkPassword: boolean;
    password: string;
    /** 平台分区号（兼容单分区旧字段；partitionNos 为空时生效） */
    partitionNo: number;
    /** 分区号列表（逗号分隔，如 "115,116,117,118"）：
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
    /** 创建时间 yyyy-MM-dd HH:mm:ss（后端维护；右键「详情」查看） */
    createdAt: string;
    /** 修改时间 yyyy-MM-dd HH:mm:ss（平台配置变化或策略热更新时后端刷新） */
    updatedAt: string;
}

/** 网关分类（与后端 config.rs 的 GatewayCategory 对应）：
 *  sz = 深圳统一网关（深交所 Binary 协议）；
 *  shjj = 上海竞价网关（上交所竞价平台 Binary 0.54 协议）；
 *  shbond = 上海新债券网关（上交所新债券平台 Binary 1.90 协议）；
 *  shzh = 上海综合网关（上交所综合业务平台 EzSTEP 数据库接口） */
export type GatewayCategory = "sz" | "shjj" | "shbond" | "shzh";

/** 网关目录（侧栏里的文件夹，与后端 config.rs 的 DirConfig 对应）：
 *  目录最多嵌套两层——根级目录（parentId 为空）内可再建子目录，
 *  子目录内不能再建目录。目录定义存后端 gateways.json，随快照下发 */
export interface DirConfig {
    /** 唯一标识（后端自动生成，新建时传空串） */
    id: string;
    /** 目录名称 */
    name: string;
    /** 父目录 id：空 = 根级目录；非空 = 挂在某个根级目录下的子目录 */
    parentId: string;
}

/** 一个网关 = 若干个平台的集合，整体启停 */
export interface GatewayConfig {
    id: string;
    name: string;
    /** 网关分类，决定网关下所有平台说哪套协议 */
    category: GatewayCategory;
    platforms: PlatformConfig[];
    /** 上次退出时是否在运行（后端 --auto-start 恢复依据，随配置存 gateways.json） */
    wasRunning: boolean;
    /** 所属目录 id（空 = 根层级，不归入任何目录） */
    dirId: string;
    /** 创建时间 yyyy-MM-dd HH:mm:ss（后端维护；旧配置缺省时启动回填当前时间） */
    createdAt: string;
    /** 修改时间 yyyy-MM-dd HH:mm:ss（配置内容变化时后端刷新；启停不视为修改） */
    updatedAt: string;
    /** 标签：逗号分隔（如 "生产,联调"），侧栏支持按标签筛选展示 */
    tags: string;
    /** 备注：自由文本说明 */
    notes: string;
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
    /** 上海综合网关：数据库当前是否连接成功（其他分类无此字段） */
    dbConnected?: boolean;
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
    /** 全部网关目录（侧栏按目录分组展示用） */
    dirs: DirConfig[];
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

/** 上海综合网关平台类型对照表（综合业务平台；STEP 消息里不使用该编号） */
export const PLATFORM_TYPES_SHZH: { value: number; label: string }[] = [
    { value: 2, label: "上海综合业务平台" },
];

/** 网关分类下拉框选项（value 必须与后端枚举的 camelCase 名一致） */
export const GATEWAY_CATEGORIES: { value: GatewayCategory; label: string }[] = [
    { value: "sz", label: "深圳统一网关" },
    { value: "shjj", label: "上海竞价网关" },
    { value: "shbond", label: "上海新债券网关" },
    { value: "shzh", label: "上海综合网关" },
];

/** 按网关分类返回对应的平台类型对照表 */
export function platformTypesOf(category: GatewayCategory): { value: number; label: string }[] {
    if (category === "shjj") return PLATFORM_TYPES_SHJJ;
    if (category === "shbond") return PLATFORM_TYPES_SHBOND;
    if (category === "shzh") return PLATFORM_TYPES_SHZH;
    return PLATFORM_TYPES;
}

/** 报单模式类别（一级下拉选项；value 与后端枚举无直接对应，只是界面归类） */
export type StrategyCategory = "fixed" | "luaScript" | "replay" | "off";

/** 一级下拉选项：固定回报模式 / Lua 脚本配置 / 回放行情撮合（待开发）/ 关闭自动回报。
 *  分类只是界面层对既有 8 个 StrategyMode 的归类（见 STRATEGY_MODES），
 *  后端枚举与 gateways.json 格式不变——"回放行情撮合"功能待开发，
 *  当前保存时按"关闭自动回报"（noAutoReply）落地 */
export const STRATEGY_CATEGORIES: { value: StrategyCategory; label: string }[] = [
    { value: "fixed", label: "固定回报模式" },
    { value: "luaScript", label: "Lua 脚本配置" },
    { value: "replay", label: "回放行情撮合（待开发）" },
    { value: "off", label: "关闭自动回报" },
];

/** 全部 8 种报单模式的标签对照表（除二级下拉外，卡片上显示模式名也用） */
export const STRATEGY_MODES: { value: StrategyMode; label: string }[] = [
    { value: "fullSingle", label: "全部成交" },
    { value: "partialSingle", label: "部分成交" },
    { value: "fullSplit", label: "全部成交（随机拆多笔）" },
    { value: "partialSplit", label: "部分成交（随机拆多笔）" },
    { value: "ackOnly", label: "只回确认回报" },
    { value: "reject", label: "自动拒单" },
    { value: "luaScript", label: "Lua 脚本回报" },
    { value: "noAutoReply", label: "关闭自动回报" },
];

/** 固定回报模式大类下的 6 种子模式（一级选"固定回报"后右侧二级下拉的选项） */
export const FIXED_STRATEGY_MODES: { value: StrategyMode; label: string }[] = STRATEGY_MODES.filter(
    (m) => m.value !== "luaScript" && m.value !== "noAutoReply",
);

/** 策略枚举值 → 所属大类（打开编辑界面时按存量配置还原一级下拉的选中项；
 *  "回放行情撮合"暂无专门枚举（保存时按 noAutoReply 落地），存量配置不会落到该类） */
export function strategyCategoryOf(mode: StrategyMode): StrategyCategory {
    if (mode === "luaScript") return "luaScript";
    if (mode === "noAutoReply") return "off";
    return "fixed";
}

/** Lua 脚本配置模式下编辑框的默认内容（即"用户默认提示词"）。
 *  合并 AI 生成输入框后，Lua 编辑器作唯一输入框：默认放一段需求示例，
 *  点「AI 生成脚本」把它变成真正的 Lua 脚本并填回同一处 */
export const DEFAULT_LUA_PROMPT = "支持输入中文需求描述，AI 会生成 Lua 脚本回报逻辑，快来试试吧！。\n示例：\n1. 只回确认回报，不成交；\n2. 价格小于 10 元的订单，随机拆成 2~5 笔成交，价格大于等于 10 元的订单，全部拒单；\n3. 委托数量小于500股，全部成交，大于500股小于1000股，按50%比例部分成交，确认回报延迟 100~500 毫秒，成交回报延迟 200~1000 毫秒。";

/** 撤单自动回报模式下拉框选项（value 与后端 CancelMode 的 camelCase 名一致） */
export const CANCEL_MODES: { value: CancelMode; label: string }[] = [
    { value: "default", label: "默认（存在挂单时自动撤单成功，否则撤单拒单）" },
    { value: "alwaysSuccess", label: "全部撤单成功" },
    { value: "alwaysReject", label: "全部撤单拒绝" },
];

/** 报单模式枚举值 → 中文标签（未知值原样返回） */
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

/** 新建平台时的默认报单自动回报模式：全部成交、无延迟；
 *  拒单参数按网关分类给默认值（深圳 20009 / 上海 1025）；
 *  撤单自动回报默认“存在挂单撤单成功、否则拒单”，无延迟 */
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
        cancelMode: "default",
        cancelRejectReason: 1,
        cancelFrontReject: false,
        cancelDelay: { minMs: 0, maxMs: 0 },
        luaScript: DEFAULT_LUA_PROMPT,
        etfPcfDir: "",
    };
}

/** 上海综合网关的默认数据库配置（新建平台时预填） */
export function defaultDbConfig(): DbConfig {
    return {
        host: "127.0.0.1",
        port: 1433,
        database: "",
        user: "sa",
        password: "",
        reqRespTable: "reqresp",
        execReportTable: "execreport",
        pollIntervalMs: 100,
    };
}

/** 平台地址摘要：TCP 网关为 IP:端口，上海综合网关为 数据库主机:端口/库名 */
export function platformAddress(p: PlatformConfig): string {
    if (p.db) return `${p.db.host}:${p.db.port}/${p.db.database}`;
    return `${p.listenHost}:${p.port}`;
}

/** 分区号按平台类型给默认值； */
export function defaultPartitionNos(platformType: number, category: GatewayCategory = "sz"): string {
    if (category === "shzh") return "";
    if (category === "shjj") return "1,2,3,4,5,6";
    if (category === "shbond") return "801";
    if (category === "sz" && platformType === 1) return "115,116,117,118";
    if (category === "sz" && platformType === 2) return "128";
    if (category === "sz" && platformType === 3) return "138";
    if (category === "sz" && platformType === 4) return "148";
    if (category === "sz" && platformType === 5) return "158";
    if (category === "sz" && platformType === 6) return "168";
    return "188";
}

/** 新建平台的默认配置（id 留空由后端生成；端口由调用方指定）。
 *  按网关分类给出不同默认值：上海竞价平台 PlatformID=0、无需密码校验。 */
export function defaultPlatform(port: number, category: GatewayCategory = "sz"): PlatformConfig {
    if (category === "shzh") {
        return {
            id: "",
            name: "上海综合业务平台",
            platformType: 2,
            listenHost: "",
            port: 0,
            db: defaultDbConfig(),
            compId: "",
            checkPassword: false,
            password: "",
            partitionNo: 0,
            partitionNos: "",
            showPackets: true,
            persistPackets: true,
            cacheOrders: true,
            createdAt: "",
            updatedAt: "",
            strategy: defaultStrategy(category),
        };
    }
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
            createdAt: "",
            updatedAt: "",
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
            createdAt: "",
            updatedAt: "",
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
        createdAt: "",
        updatedAt: "",
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
