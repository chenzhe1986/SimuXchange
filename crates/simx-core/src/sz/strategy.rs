//! 深交所模拟回报策略：根据配置为一笔委托生成回报计划（确认 + 成交 / 拒单）
//!
//! # 职责
//!
//! 模拟器不做真实撮合，而是按平台配置的策略生成回报计划：输入
//! 一笔委托 + 策略配置，输出一串 PlannedReport（每条含发送前等待
//! 时长与报文内容），由 session 模块按计划发送。
//!
//! # 多业务支持（表 3-3 / 表 3-4）
//!
//! 4.5.1 的 27 种新订单业务共用同一套策略框架，通过 `biz_info`
//! 业务特征表区分差异：
//! - **确认/成交报告报文类型**：按业务分别用 4.5.4 / 4.5.5 的消息号
//! - **有无成交回报**（表 3-4）：部分业务只回确认，成交类策略自动失效
//! - **是否支持部分成交**：仅现货集中竞价（010）支持；其余业务把
//!   PartialSingle/PartialSplit/Custom 等部分成交策略降级为全部成交
//!
//! 七种策略与对应剧本：
//! - FullSingle  全部成交（单笔）：1 确认 + 1 成交
//! - FullSplit   全部成交（拆单）：1 确认 + N 成交（数量随机拆、价格阶梯）
//! - PartialSingle 部分成交（单笔）：1 确认 + 1 成交（数量小于委托量）
//! - PartialSplit  部分成交（拆单）：1 确认 + N 成交（总量小于委托量）
//! - Custom      自定义：1 确认 + 用户逐笔指定的成交（数量/价格）
//! - AckOnly     只确认不成交：1 确认（挂单状态）
//! - Reject      拒单：1 条拒绝（可选拒单回报 2xxx02 或业务拒绝 4）

use crate::config::{RejectVia, StrategyConfig, StrategyMode};
use crate::orderbook::{OrderStatus, OrderUpdate};
use super::protocol::{
    self as protocol, exec_type, msg_type, ord_status, BusinessReject, ExecRptAck, ExecRptTrade,
    NewOrder,
};
use crate::stats::PlatformStats;
use rand::Rng;

/// 回报种类（用于统计计数与日志）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// 确认回报（委托已被交易所接受）
    Ack,
    /// 成交回报
    Trade,
    /// 拒单（以执行回报 2xxx02 形式）
    Reject,
    /// 业务拒绝（以独立消息 MsgType=4 形式）
    BusinessReject,
    /// 撤单成功回报（仅手动回复产生；不占自动策略的统计计数）
    Cancel,
}

/// 一条待发送的回报
pub struct PlannedReport {
    /// 发送前等待的毫秒数（相对上一条回报）
    pub delay_ms: u64,
    pub kind: ReportKind,
    /// 完整报文（含消息头尾）
    pub frame: Vec<u8>,
    /// 日志描述
    pub desc: String,
    /// 本条回报属于哪笔委托（订单缓存更新与统计用）
    pub cl_ord_id: String,
    /// 本条回报发出后要同步给订单缓存的更新（None = 无需更新）
    pub order_update: Option<OrderUpdate>,
}

/// Qty N15(2)：协议中数量放大 100 倍存储，1 股 = 100
const QTY_UNIT: i64 = 100;
/// Price N13(4)：协议中价格放大 10000 倍存储，1 元 = 10000
const PX_UNIT: f64 = 10000.0;
/// 深市股票一手 = 100 股（拆单时尽量按整手拆）
const LOT: i64 = 100;

/// 一个业务的模拟特征（表 3-3 委托申报类型 + 表 3-4 有无成交回报汇总）。
///
/// 每种新订单消息类型对应一个业务，策略模块按它决定回什么报文。
#[derive(Debug, Clone, Copy)]
pub struct BizInfo {
    /// 业务名称（日志/展示用）
    pub name: &'static str,
    /// 应用标识 ApplID（表 3-3；委托里 ApplID 为空时的兜底值）
    pub appl_id: &'static str,
    /// 确认执行报告消息类型（4.5.4，MsgType = 2xxx02）
    pub ack_msg_type: u32,
    /// 成交执行报告消息类型（4.5.5，MsgType = 2xxx15）；
    /// None = 该业务无成交回报（表 3-4），成交类策略自动失效
    pub trade_msg_type: Option<u32>,
    /// 是否支持部分成交（仅现货集中竞价 010 支持；其余业务不支持，
    /// 部分成交类策略会被降级为全部成交）
    pub allow_partial: bool,
}

/// 业务特征表：按规范 4.5.1.x 小节顺序映射 27 种新订单消息类型。
///
/// 有成交回报的业务（表 3-4）：010/020/030/040/051/052/060/061/070/630/370/410/417；
/// 其余业务只有确认执行报告（120/130/131/132/140/150/151/152/160/170/180/181/
/// 190/191/220/230/270/271/280/281/290/291/310/311/330/331/350/351/470）。
pub fn biz_info(mt: u32) -> BizInfo {
    match mt {
        // 4.5.1.1 现货集中竞价（010）：唯一支持部分成交的业务
        msg_type::NEW_ORDER_CASH => BizInfo {
            name: "现货集中竞价",
            appl_id: "010",
            ack_msg_type: msg_type::EXEC_RPT_CASH_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_CASH_TRADE),
            allow_partial: true,
        },
        // 4.5.1.2 债券通用质押式回购（020）
        msg_type::NEW_ORDER_BOND_REPO => BizInfo {
            name: "债券通用质押式回购",
            appl_id: "020",
            ack_msg_type: msg_type::EXEC_RPT_BOND_REPO_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_BOND_REPO_TRADE),
            allow_partial: false,
        },
        // 表 3-3 债券分销（030）
        msg_type::NEW_ORDER_BOND_DIST => BizInfo {
            name: "债券分销",
            appl_id: "030",
            ack_msg_type: msg_type::EXEC_RPT_BOND_DIST_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_BOND_DIST_TRADE),
            allow_partial: false,
        },
        // 4.5.1.3 期权集中竞价（040）
        msg_type::NEW_ORDER_OPTION_AUCTION => BizInfo {
            name: "期权集中竞价",
            appl_id: "040",
            ack_msg_type: msg_type::EXEC_RPT_OPTION_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_OPTION_TRADE),
            allow_partial: false,
        },
        // 4.5.1.4 协议交易（051/052）
        msg_type::NEW_ORDER_AGREEMENT_TRADE => BizInfo {
            name: "协议交易",
            appl_id: "051",
            ack_msg_type: msg_type::EXEC_RPT_AGREEMENT_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_AGREEMENT_TRADE),
            allow_partial: false,
        },
        // 4.5.1.5 盘后定价大宗交易（060/061）
        msg_type::NEW_ORDER_BLOCK_TRADE => BizInfo {
            name: "盘后定价大宗交易",
            appl_id: "060",
            ack_msg_type: msg_type::EXEC_RPT_BLOCK_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_BLOCK_TRADE),
            allow_partial: false,
        },
        // 4.5.1.6 转融通证券出借（070）
        msg_type::NEW_ORDER_SEC_LENDING => BizInfo {
            name: "转融通证券出借",
            appl_id: "070",
            ack_msg_type: msg_type::EXEC_RPT_SEC_LENDING_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_SEC_LENDING_TRADE),
            allow_partial: false,
        },
        // 4.5.1.19 ETF 实时申购赎回（120）：无成交回报
        msg_type::NEW_ORDER_ETF_SUB_RED => BizInfo {
            name: "ETF实时申购赎回",
            appl_id: "120",
            ack_msg_type: msg_type::EXEC_RPT_ETF_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 表 3-3 网上发行认购（130/131/132）：无成交回报
        msg_type::NEW_ORDER_ISSUE => BizInfo {
            name: "网上发行认购",
            appl_id: "130",
            ack_msg_type: msg_type::EXEC_RPT_ISSUE_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 表 3-3 配股认购（140）：无成交回报
        msg_type::NEW_ORDER_RIGHTS => BizInfo {
            name: "配股认购",
            appl_id: "140",
            ack_msg_type: msg_type::EXEC_RPT_RIGHTS_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.7 债券转股回售（150/151/152）：无成交回报
        msg_type::NEW_ORDER_BOND_CONVERT => BizInfo {
            name: "债券转股回售",
            appl_id: "150",
            ack_msg_type: msg_type::EXEC_RPT_BOND_CONVERT_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.8 期权行权（160）：无成交回报
        msg_type::NEW_ORDER_OPTION_EXERCISE => BizInfo {
            name: "期权行权",
            appl_id: "160",
            ack_msg_type: msg_type::EXEC_RPT_OPTION_EXERCISE_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.9 开放式基金申购赎回（170）：无成交回报
        msg_type::NEW_ORDER_FUND_SUB_RED => BizInfo {
            name: "开放式基金申购赎回",
            appl_id: "170",
            ack_msg_type: msg_type::EXEC_RPT_FUND_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.10 要约收购（180/181）：无成交回报
        msg_type::NEW_ORDER_TENDER_OFFER => BizInfo {
            name: "要约收购",
            appl_id: "180",
            ack_msg_type: msg_type::EXEC_RPT_TENDER_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 表 3-3 债券通用质押式回购质押/解押（190/191）：无成交回报
        msg_type::NEW_ORDER_REPO_PLEDGE => BizInfo {
            name: "回购质押解押",
            appl_id: "190",
            ack_msg_type: msg_type::EXEC_RPT_REPO_PLEDGE_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 表 3-3 黄金 ETF 实物申购赎回（220）：无成交回报
        msg_type::NEW_ORDER_GOLD_ETF => BizInfo {
            name: "黄金ETF实物申购赎回",
            appl_id: "220",
            ack_msg_type: msg_type::EXEC_RPT_GOLD_ETF_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 表 3-3 权证行权（230）：无成交回报
        msg_type::NEW_ORDER_WARRANT => BizInfo {
            name: "权证行权",
            appl_id: "230",
            ack_msg_type: msg_type::EXEC_RPT_WARRANT_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.11 转处置（270/271）：无成交回报
        msg_type::NEW_ORDER_DISPOSAL => BizInfo {
            name: "转处置",
            appl_id: "270",
            ack_msg_type: msg_type::EXEC_RPT_DISPOSAL_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.12 垫券还券（280/281）：无成交回报
        msg_type::NEW_ORDER_LEND_RETURN => BizInfo {
            name: "垫券还券",
            appl_id: "280",
            ack_msg_type: msg_type::EXEC_RPT_LEND_RETURN_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.13 待清偿扣划（290/291）：无成交回报
        msg_type::NEW_ORDER_DEDUCTION => BizInfo {
            name: "待清偿扣划",
            appl_id: "290",
            ack_msg_type: msg_type::EXEC_RPT_DEDUCTION_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 表 3-3 分级基金实时分拆/合并（310/311）：无成交回报
        msg_type::NEW_ORDER_SPLIT_MERGE => BizInfo {
            name: "分级基金实时分拆合并",
            appl_id: "310",
            ack_msg_type: msg_type::EXEC_RPT_SPLIT_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 表 3-3 债券质押式三方回购入库/出库（330/331）：无成交回报
        msg_type::NEW_ORDER_3P_REPO => BizInfo {
            name: "债券质押式三方回购入库出库",
            appl_id: "330",
            ack_msg_type: msg_type::EXEC_RPT_3P_REPO_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.15 期权普通与备兑仓互转（350/351）：无成交回报
        msg_type::NEW_ORDER_OPTION_CONVERT => BizInfo {
            name: "期权普通与备兑仓互转",
            appl_id: "350",
            ack_msg_type: msg_type::EXEC_RPT_OPTION_CONVERT_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.16 盘后定价交易（370）
        msg_type::NEW_ORDER_AFTER_HOURS => BizInfo {
            name: "盘后定价交易",
            appl_id: "370",
            ack_msg_type: msg_type::EXEC_RPT_AFTER_HOURS_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_AFTER_HOURS_TRADE),
            allow_partial: false,
        },
        // 4.5.1.17(1) 债券现券交易匹配成交（410）
        msg_type::NEW_ORDER_BOND_CASH => BizInfo {
            name: "债券现券匹配成交",
            appl_id: "410",
            ack_msg_type: msg_type::EXEC_RPT_BOND_CASH_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_BOND_CASH_TRADE),
            allow_partial: false,
        },
        // 4.5.1.17(2) 债券现券交易竞买成交（417）
        msg_type::NEW_ORDER_BOND_BID => BizInfo {
            name: "债券现券竞买成交",
            appl_id: "417",
            ack_msg_type: msg_type::EXEC_RPT_BOND_BID_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_BOND_BID_TRADE),
            allow_partial: false,
        },
        // 4.5.1.18 跨银行间实物债券 ETF 实物申购赎回（470）：无成交回报
        msg_type::NEW_ORDER_INTERBANK_ETF => BizInfo {
            name: "跨银行间ETF实物申购赎回",
            appl_id: "470",
            ack_msg_type: msg_type::EXEC_RPT_INTERBANK_ETF_ACK,
            trade_msg_type: None,
            allow_partial: false,
        },
        // 4.5.1.14 港股通（630）
        msg_type::NEW_ORDER_HK_CONNECT => BizInfo {
            name: "港股通",
            appl_id: "630",
            ack_msg_type: msg_type::EXEC_RPT_HK_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_HK_TRADE),
            allow_partial: false,
        },
        // 未知消息类型：按现货竞价兜底（正常流程不会走到）
        _ => BizInfo {
            name: "未知业务",
            appl_id: "010",
            ack_msg_type: msg_type::EXEC_RPT_CASH_ACK,
            trade_msg_type: Some(msg_type::EXEC_RPT_CASH_TRADE),
            allow_partial: false,
        },
    }
}

/// 按应用标识 ApplID 反查业务特征（撤单成功回报/手动回报按订单缓存的 ApplID 定位业务）。
///
/// 表 3-3 中同一消息类型可对应多个 ApplID（第三位表示委托申报代码），
/// 例如 100501 同时服务 051 定价 / 052 点击成交；这里把同业务的多值合并。
/// 查不到时返回 None（兜底由调用方决定）。
pub fn biz_info_by_appl_id(appl_id: &str) -> Option<BizInfo> {
    let mt = match appl_id {
        "010" => msg_type::NEW_ORDER_CASH,
        "020" => msg_type::NEW_ORDER_BOND_REPO,
        "030" => msg_type::NEW_ORDER_BOND_DIST,
        "040" => msg_type::NEW_ORDER_OPTION_AUCTION,
        // 100501：051 定价 / 052 点击成交
        "051" | "052" => msg_type::NEW_ORDER_AGREEMENT_TRADE,
        // 100601：060 收盘价 / 061 VWAP
        "060" | "061" => msg_type::NEW_ORDER_BLOCK_TRADE,
        "070" => msg_type::NEW_ORDER_SEC_LENDING,
        "120" => msg_type::NEW_ORDER_ETF_SUB_RED,
        // 101301：130 发行增发 / 131 非定向扩募配售 / 132 非定向扩募公开发售
        "130" | "131" | "132" => msg_type::NEW_ORDER_ISSUE,
        "140" => msg_type::NEW_ORDER_RIGHTS,
        // 101501：150 转股 / 151 回售 / 152 回售撤销
        "150" | "151" | "152" => msg_type::NEW_ORDER_BOND_CONVERT,
        "160" => msg_type::NEW_ORDER_OPTION_EXERCISE,
        "170" => msg_type::NEW_ORDER_FUND_SUB_RED,
        // 101801：180 预受要约 / 181 解除预受
        "180" | "181" => msg_type::NEW_ORDER_TENDER_OFFER,
        // 101901：190 质押 / 191 解押
        "190" | "191" => msg_type::NEW_ORDER_REPO_PLEDGE,
        "220" => msg_type::NEW_ORDER_GOLD_ETF,
        "230" => msg_type::NEW_ORDER_WARRANT,
        // 102701：270 扣券 / 271 还券
        "270" | "271" => msg_type::NEW_ORDER_DISPOSAL,
        // 102801：280 垫券 / 281 还券
        "280" | "281" => msg_type::NEW_ORDER_LEND_RETURN,
        // 102901：290 客户 / 291 自营
        "290" | "291" => msg_type::NEW_ORDER_DEDUCTION,
        // 103101：310 分拆 / 311 合并
        "310" | "311" => msg_type::NEW_ORDER_SPLIT_MERGE,
        // 103301：330 入库 / 331 出库
        "330" | "331" => msg_type::NEW_ORDER_3P_REPO,
        // 103501：350 普通转备兑 / 351 备兑转普通
        "350" | "351" => msg_type::NEW_ORDER_OPTION_CONVERT,
        "370" => msg_type::NEW_ORDER_AFTER_HOURS,
        "410" => msg_type::NEW_ORDER_BOND_CASH,
        "417" => msg_type::NEW_ORDER_BOND_BID,
        "470" => msg_type::NEW_ORDER_INTERBANK_ETF,
        "630" => msg_type::NEW_ORDER_HK_CONNECT,
        _ => return None,
    };
    Some(biz_info(mt))
}

/// 应用标识 → 所属业务平台（表 3-1/表 3-3 各平台业务划分）。
///
/// 平台号与 5.3 平台状态/5.5 平台信息中的 PlatformID 一致：
/// 1=现货集中竞价交易平台 2=综合金融服务平台 3=非交易处理平台
/// 4=衍生品集中竞价交易平台 5=国际市场互联平台 6=固定收益交易平台。
/// 业务申报的 ApplID 与当前接入平台不符时，应回 20108 业务拒绝（见 session 处理）。
pub fn platform_of_appl_id(appl_id: &str) -> Option<u16> {
    let p = match appl_id.trim_end() {
        // 平台 1 现货集中竞价
        "010" => 1,
        // 平台 2 综合金融（协议交易/盘后定价大宗/转融通出借/份额转让/质押回购/约定购回/报价回购/盘后定价）
        "050" | "051" | "052" | "053" | "055" | "056" | "057" | "060" | "061" | "070"
        | "071" | "080" | "090" | "100" | "110" | "370" => 2,
        // 平台 3 非交易处理（ETF申赎/发行/配股/转股回售/期权行权/基金申赎/要约/转托管/投票/密码/权证/保证金/转处置/垫券还券/扣划/分拆合并/跨银行间ETF）
        "120" | "130" | "131" | "132" | "140" | "150" | "151" | "152" | "160" | "161"
        | "170" | "180" | "181" | "200" | "210" | "220" | "230" | "240" | "241"
        | "250" | "270" | "271" | "280" | "281" | "290" | "291" | "310" | "311"
        | "470" => 3,
        // 平台 4 衍生品集中竞价（期权竞价/组合策略/普通备兑互转）
        "040" | "041" | "042" | "340" | "341" | "350" | "351" => 4,
        // 平台 5 国际市场互联
        "630" => 5,
        // 平台 6 固定收益（协议回购/三方回购/质押解押/转让/借贷/分销/通用质押式回购/现券交易）
        "020" | "030" | "190" | "191" | "300" | "320" | "330" | "331" | "410" | "411"
        | "412" | "413" | "414" | "415" | "416" | "417" | "419" | "41A" | "420"
        | "430" => 6,
        _ => return None,
    };
    Some(p)
}

/// 把“元”换算成协议的放大整数（如 10.01 元 → 100100）
fn px_raw(price_yuan: f64) -> i64 {
    (price_yuan * PX_UNIT).round() as i64
}

/// 把“股数”换算成协议的放大整数（如 100 股 → 10000）
fn qty_raw(shares: i64) -> i64 {
    shares * QTY_UNIT
}

/// 为一笔新订单生成回报计划（本模块唯一对外入口）。
///
/// 总体逻辑：
/// 1. 拒单策略 → 只生成 1 条拒绝，直接返回
/// 2. 其他策略 → 先生成 1 条确认，再由 gen_fills 算出成交明细，
///    逐笔生成成交回报（累计成交量/剩余量/订单状态逐步推进）
/// 3. 按业务特征（biz_info）决定确认/成交报告的报文类型；
///    无成交回报的业务跳过成交环节，只回确认
///
/// st 为策略配置（可来自运行时热更新的共享区）、partition_no 为平台分区号，
/// 两条回报的延迟由 st 的 ack_delay / trade_delay 抽样得到。
pub fn plan_reports(
    st: &StrategyConfig,
    partition_no: i32,
    order: &NewOrder,
    stats: &PlatformStats,
) -> Vec<PlannedReport> {
    // 业务特征：决定回报报文类型与是否有成交回报
    let biz = biz_info(order.msg_type);
    let order_id = stats.next_order_id();
    let mut plans = Vec::new();

    // ---- 拒单策略：只回一条拒绝，没有确认也没有成交 ----
    if st.mode == StrategyMode::Reject {
        match st.reject_via {
            // 方式一：用执行报告(2xxx02)拒单，在确认报文基础上
            // 把执行类型/订单状态改成“已拒绝”并带上拒绝原因代码
            RejectVia::ExecutionReport => {
                let mut rpt = base_ack(partition_no, order, &biz, stats, &order_id);
                rpt.exec_type = exec_type::REJECT;
                rpt.ord_status = ord_status::REJECTED;
                rpt.ord_rej_reason = st.reject_reason;
                rpt.leaves_qty = 0;
                rpt.cum_qty = 0;
                plans.push(PlannedReport {
                    delay_ms: st.ack_delay.sample(),
                    kind: ReportKind::Reject,
                    frame: rpt.encode(),
                    desc: format!(
                        "拒单回报({}) ClOrdID={} 原因代码={}",
                        biz.ack_msg_type, order.common.cl_ord_id, st.reject_reason
                    ),
                    cl_ord_id: order.common.cl_ord_id.clone(),
                    order_update: Some(OrderUpdate {
                        order_id: order_id.clone(),
                        cum_qty: 0.0,
                        leaves_qty: 0.0,
                        status: OrderStatus::Rejected,
                    }),
                });
            }
            // 方式二：用独立的业务拒绝消息(MsgType=4)，可携带文字说明
            RejectVia::BusinessReject => {
                let rej = BusinessReject {
                    appl_id: fill_or(&order.common.appl_id, biz.appl_id),
                    transact_time: protocol::now_timestamp(),
                    submitting_pbu_id: order.common.submitting_pbu_id.clone(),
                    security_id: order.common.security_id.clone(),
                    security_id_source: order.common.security_id_source.clone(),
                    ref_seq_num: 0,
                    ref_msg_type: order.msg_type,
                    business_reject_ref_id: order.common.cl_ord_id.clone(),
                    business_reject_reason: st.reject_reason,
                    business_reject_text: st.reject_text.clone(),
                };
                plans.push(PlannedReport {
                    delay_ms: st.ack_delay.sample(),
                    kind: ReportKind::BusinessReject,
                    frame: rej.encode(),
                    desc: format!(
                        "业务拒绝(4) ClOrdID={} 原因={}",
                        order.common.cl_ord_id, st.reject_text
                    ),
                    cl_ord_id: order.common.cl_ord_id.clone(),
                    order_update: Some(OrderUpdate {
                        order_id: String::new(), // 业务拒绝未分配交易所订单号
                        cum_qty: 0.0,
                        leaves_qty: 0.0,
                        status: OrderStatus::Rejected,
                    }),
                });
            }
        }
        return plans;
    }

    // ---- 确认回报：除拒单外所有策略都先发一条确认 ----
    let ack = base_ack(partition_no, order, &biz, stats, &order_id);
    plans.push(PlannedReport {
        delay_ms: st.ack_delay.sample(),
        kind: ReportKind::Ack,
        frame: ack.encode(),
        desc: format!(
            "确认回报({}) ClOrdID={} OrderID={}",
            biz.ack_msg_type,
            order.common.cl_ord_id,
            order_id.trim_start_matches('0')
        ),
        cl_ord_id: order.common.cl_ord_id.clone(),
        order_update: Some(OrderUpdate {
            order_id: order_id.clone(),
            cum_qty: 0.0,
            leaves_qty: order.common.order_qty as f64 / QTY_UNIT as f64,
            status: OrderStatus::New,
        }),
    });

    // ---- 成交回报：按策略算出的成交明细逐笔生成 ----
    // 表 3-4 无成交回报的业务（trade_msg_type = None）直接跳过，
    // 订单保持“已报”挂单状态，由柜台后续撤销或等待人工处理
    let Some(trade_msg_type) = biz.trade_msg_type else {
        return plans;
    };
    let fills = gen_fills(st, order, &biz);
    let total: i64 = order.common.order_qty;
    let mut cum: i64 = 0; // 累计已成交数量（协议字段 CumQty）
    let n = fills.len();
    for (i, (fill_qty, fill_px)) in fills.into_iter().enumerate() {
        cum += fill_qty;
        // 剩余未成交数量；最后一笔成交后为 0 → 订单状态变为“全部成交”
        let leaves = (total - cum).max(0);
        let status = if leaves == 0 {
            ord_status::FILLED
        } else {
            ord_status::PARTIALLY_FILLED
        };
        let trade = build_trade(
            partition_no, order, &biz, stats, &order_id, trade_msg_type,
            fill_qty, fill_px, cum, leaves, status,
        );
        plans.push(PlannedReport {
            delay_ms: st.trade_delay.sample(),
            kind: ReportKind::Trade,
            frame: trade.encode(),
            desc: format!(
                "成交回报({}) ClOrdID={} 第{}/{}笔 价格={:.4} 数量={} 剩余={}",
                trade_msg_type,
                order.common.cl_ord_id,
                i + 1,
                n,
                fill_px as f64 / PX_UNIT,
                fill_qty / QTY_UNIT,
                leaves / QTY_UNIT
            ),
            cl_ord_id: order.common.cl_ord_id.clone(),
            order_update: Some(OrderUpdate {
                order_id: order_id.clone(),
                cum_qty: cum as f64 / QTY_UNIT as f64,
                leaves_qty: leaves as f64 / QTY_UNIT as f64,
                status: if leaves == 0 {
                    OrderStatus::Filled
                } else {
                    OrderStatus::Partial
                },
            }),
        });
    }

    plans
}

/// 无成交回报业务：订单保持已报状态，模拟真实柜台“申报成功、等待后续处理”的场景。

/// 小工具：v 为空时用默认值顶上（某些柜台不填可选字段）
fn fill_or(v: &str, default: &str) -> String {
    if v.is_empty() {
        default.to_string()
    } else {
        v.to_string()
    }
}

/// 构造确认回报（ExecType=0 已报）：大部分字段直接回填委托里的值，
/// 再填上交易所分配的订单号/执行号/回报序号；扩展字段整体拷贝委托的
/// 扩展字段（编码时按消息类型只写该业务的相关字段）。
/// 确认时尚未成交：剩余量 = 委托量，累计成交量 = 0。
fn base_ack(
    partition_no: i32,
    order: &NewOrder,
    biz: &BizInfo,
    stats: &PlatformStats,
    order_id: &str,
) -> ExecRptAck {
    let c = &order.common;
    ExecRptAck {
        msg_type: biz.ack_msg_type,
        partition_no,
        report_index: 0, // 发送时由 writer 任务补写（保证与线上顺序一致）
        appl_id: fill_or(&c.appl_id, biz.appl_id),
        reporting_pbu_id: c.submitting_pbu_id.clone(),
        submitting_pbu_id: c.submitting_pbu_id.clone(),
        security_id: c.security_id.clone(),
        security_id_source: fill_or(&c.security_id_source, "102"),
        owner_type: c.owner_type,
        clearing_firm: c.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: c.user_info.clone(),
        order_id: order_id.to_string(),
        cl_ord_id: c.cl_ord_id.clone(),
        orig_cl_ord_id: String::new(),
        exec_id: stats.next_exec_id(),
        exec_type: exec_type::NEW,
        ord_status: ord_status::NEW,
        ord_rej_reason: 0,
        leaves_qty: c.order_qty,
        cum_qty: 0,
        side: c.side,
        ord_type: c.ord_type,
        order_qty: c.order_qty,
        price: c.price,
        account_id: c.account_id.clone(),
        branch_id: c.branch_id.clone(),
        order_restrictions: c.order_restrictions.clone(),
        extend: order.extend.clone(),
    }
}

/// 构造一笔成交回报（ExecType=F 成交）。
/// 公共字段回填委托/订单信息，扩展字段整体拷贝委托的扩展字段——
/// 编码时按业务消息类型只写该业务相关的字段（如 200115 只写 CashMargin、
/// 200415 写期权四字段），委托里没有的字段（如对手方信息、到期日）
/// 保持默认值 0/空串。
#[allow(clippy::too_many_arguments)]
fn build_trade(
    partition_no: i32,
    order: &NewOrder,
    biz: &BizInfo,
    stats: &PlatformStats,
    order_id: &str,
    msg_type: u32,
    fill_qty: i64,
    fill_px: i64,
    cum: i64,
    leaves: i64,
    status: u8,
) -> ExecRptTrade {
    let c = &order.common;
    ExecRptTrade {
        msg_type,
        partition_no,
        report_index: 0, // 发送时由 writer 任务补写（保证与线上顺序一致）
        appl_id: fill_or(&c.appl_id, biz.appl_id),
        reporting_pbu_id: c.submitting_pbu_id.clone(),
        submitting_pbu_id: c.submitting_pbu_id.clone(),
        security_id: c.security_id.clone(),
        security_id_source: fill_or(&c.security_id_source, "102"),
        owner_type: c.owner_type,
        clearing_firm: c.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: c.user_info.clone(),
        order_id: order_id.to_string(),
        cl_ord_id: c.cl_ord_id.clone(),
        exec_id: stats.next_exec_id(),
        exec_type: exec_type::TRADE,
        ord_status: status,
        last_px: fill_px,
        last_qty: fill_qty,
        leaves_qty: leaves,
        cum_qty: cum,
        side: c.side,
        account_id: c.account_id.clone(),
        branch_id: c.branch_id.clone(),
        extend: order.extend.clone(),
    }
}

/// 生成成交明细列表：每项为 (数量 raw, 价格 raw)，均为协议放大整数。
/// 这里只决定“成交几笔、每笔多少股、什么价”，不管报文细节。
///
/// 差异点：除现货集中竞价（010）外的业务不支持部分成交，把
/// PartialSingle/PartialSplit/Custom 降级为对应的全部成交策略。
fn gen_fills(st: &StrategyConfig, order: &NewOrder, biz: &BizInfo) -> Vec<(i64, i64)> {
    let total_shares = order.common.order_qty / QTY_UNIT; // 委托股数
    // 非竞价业务的部分成交降级：PartialSingle→FullSingle、PartialSplit→FullSplit、
    // Custom→FullSingle（自定义明细可能出现“未成交完”的部分成交语义）
    let mode = if biz.allow_partial {
        st.mode
    } else {
        match st.mode {
            StrategyMode::PartialSingle => StrategyMode::FullSingle,
            StrategyMode::PartialSplit => StrategyMode::FullSplit,
            StrategyMode::Custom => StrategyMode::FullSingle,
            m => m,
        }
    };
    match mode {
        // 全部成交（单笔）：一笔成交全部数量，价格就是委托价
        StrategyMode::FullSingle => {
            vec![(order.common.order_qty, order.common.price)]
        }
        // 全部成交（拆单）：先随机拆成若干笔，再配阶梯价
        StrategyMode::FullSplit => {
            let qtys = split_shares(total_shares, sample_split_count(st));
            with_ladder_prices(st, order, &qtys)
        }
        // 部分成交（单笔）：随机取一个小于委托量的数量
        StrategyMode::PartialSingle => {
            let part = partial_shares(total_shares);
            vec![(qty_raw(part), order.common.price)]
        }
        // 部分成交（拆单）：先定部分成交总量，再拆单配阶梯价
        StrategyMode::PartialSplit => {
            let part = partial_shares(total_shares);
            let qtys = split_shares(part, sample_split_count(st));
            with_ladder_prices(st, order, &qtys)
        }
        // 自定义：直接用用户在界面上填的逐笔数量/价格（跳过数量为 0 的行）。
        // 累计成交量钳制到委托量以内：明细总和超过委托量时截断（否则回报里
        // CumQty 会超 OrderQty，协议非法）；价格下限 0.0001 元（0 价成交无效）
        StrategyMode::Custom => {
            let mut remaining = total_shares;
            let mut out = Vec::new();
            for f in st.custom_fills.iter().filter(|f| f.qty > 0.0) {
                if remaining <= 0 {
                    break;
                }
                let shares = (f.qty.round() as i64).clamp(1, remaining);
                remaining -= shares;
                out.push((qty_raw(shares), px_raw(f.price.max(0.0001))));
            }
            out
        }
        // 只确认/拒单：没有成交
        StrategyMode::AckOnly | StrategyMode::Reject => Vec::new(),
    }
}

/// 拆单笔数取样：在配置的 [min, max] 区间内随机取一个数。
/// 先容错：若用户把 min/max 填反了就互换，且保证至少为 1
fn sample_split_count(st: &StrategyConfig) -> u32 {
    let (lo, hi) = if st.split_count_min <= st.split_count_max {
        (st.split_count_min.max(1), st.split_count_max.max(1))
    } else {
        (st.split_count_max.max(1), st.split_count_min.max(1))
    };
    if lo == hi {
        lo
    } else {
        rand::thread_rng().gen_range(lo..=hi)
    }
}

/// 部分成交数量：按手随机（至少 1 手，最多 总数-1 手），
/// 保证一定“没成交完”，才能体现“部分成交”的语义
fn partial_shares(total_shares: i64) -> i64 {
    let units = total_shares / LOT;
    if units >= 2 {
        LOT * rand::thread_rng().gen_range(1..=units - 1)
    } else if total_shares > 1 {
        // 不足 2 手时取一半（向下取整，至少 1 股）
        (total_shares / 2).max(1)
    } else {
        total_shares.max(1)
    }
}

/// 将 total_shares 股随机拆成 n 笔（尽量按整手拆分），返回每笔股数。
/// 拆分算法：前 n-1 笔每笔随机拿若干整手（为后面每笔至少留 1 手），
/// 最后一笔拿走剩余全部，保证各笔之和恰好等于总数。
fn split_shares(total_shares: i64, n: u32) -> Vec<i64> {
    let mut rng = rand::thread_rng();
    let units = total_shares / LOT;
    let n = (n as i64).clamp(1, units.max(1)) as usize;
    if n <= 1 || units <= 1 {
        return vec![total_shares];
    }
    let mut result = Vec::with_capacity(n);
    let mut remaining_units = units;
    for i in 0..n - 1 {
        // 为剩余的每一笔至少保留 1 手
        let reserve = (n - 1 - i) as i64;
        let max_take = remaining_units - reserve;
        let take = rng.gen_range(1..=max_take);
        result.push(take * LOT);
        remaining_units -= take;
    }
    // 最后一笔拿走剩余整手 + 零头
    result.push(remaining_units * LOT + (total_shares - units * LOT));
    result
}

/// 为拆单成交生成价格阶梯：
/// 买单按价格档位递增、卖单递减，最后一笔正好等于委托价（不越过限价）。
/// 这模拟了真实市场“先吃掉便宜的对手盘、再逐档向限价靠拢”的成交过程。
fn with_ladder_prices(
    st: &StrategyConfig,
    order: &NewOrder,
    qtys: &[i64],
) -> Vec<(i64, i64)> {
    let tick = px_raw(st.price_tick).max(1);
    let n = qtys.len() as i64;
    qtys.iter()
        .enumerate()
        .map(|(i, &shares)| {
            let steps = n - 1 - i as i64;
            let px = if order.common.side == b'2' {
                // 卖单：从高到低递减至委托价
                order.common.price + tick * steps
            } else {
                // 买单：从低到高递增至委托价
                (order.common.price - tick * steps).max(tick)
            };
            (qty_raw(shares), px)
        })
        .collect()
}

// 单元测试：验证各策略生成的回报数量、拆单总量、阶梯价等关键规则。
// 运行方式：cargo test -p simx-core
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CustomFill, DelayConfig, PlatformConfig};
    use crate::sz::protocol::OrderCommon;

    /// 造一笔指定业务/策略的测试委托（数量单位：股；价格单位：元）
    fn mk_order(mt: u32, qty_shares: i64, price: f64, side: u8) -> NewOrder {
        let biz = biz_info(mt);
        NewOrder {
            msg_type: mt,
            common: OrderCommon {
                appl_id: biz.appl_id.into(),
                submitting_pbu_id: "100001".into(),
                security_id: "000001".into(),
                security_id_source: "102".into(),
                cl_ord_id: "CL001".into(),
                side,
                ord_type: b'2',
                order_qty: qty_shares * QTY_UNIT,
                price: px_raw(price),
                ..Default::default()
            },
            extend: Default::default(),
        }
    }

    /// 造一份指定策略的平台配置（固定拆 3 笔、档位 0.01 元，便于断言）
    fn mk_cfg(mode: StrategyMode) -> PlatformConfig {
        PlatformConfig {
            strategy: StrategyConfig {
                mode,
                split_count_min: 3,
                split_count_max: 3,
                price_tick: 0.01,
                custom_fills: vec![
                    CustomFill { qty: 300.0, price: 10.01 },
                    CustomFill { qty: 200.0, price: 10.02 },
                ],
                ack_delay: DelayConfig::default(),
                trade_delay: DelayConfig::default(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_full_single() {
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_CASH, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::FullSingle);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 2); // ack + 1 笔成交
        assert_eq!(plans[0].kind, ReportKind::Ack);
        assert_eq!(plans[1].kind, ReportKind::Trade);
    }

    #[test]
    fn test_ack_and_trade_msg_type_follow_business() {
        // 债券回购（020）：确认 200202、成交 200215
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_BOND_REPO, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::FullSingle);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 2);
        assert_eq!(u32::from_be_bytes(plans[0].frame[0..4].try_into().unwrap()), 200_202);
        assert_eq!(u32::from_be_bytes(plans[1].frame[0..4].try_into().unwrap()), 200_215);
    }

    #[test]
    fn test_non_cash_business_downgrades_partial_to_full() {
        // 非竞价业务 + PartialSingle：降级为全部成交（数量=委托量）
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_BOND_REPO, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::PartialSingle);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 2);
        let trade = ExecRptTrade::decode(
            msg_type::EXEC_RPT_BOND_REPO_TRADE,
            &plans[1].frame[8..plans[1].frame.len() - 4],
        )
        .unwrap();
        assert_eq!(trade.last_qty, 1000 * QTY_UNIT); // 全部成交
        assert_eq!(trade.ord_status, ord_status::FILLED);
    }

    #[test]
    fn test_business_without_trade_ack_only() {
        // 无成交回报业务（ETF 实时申赎 120）+ FullSingle：只回确认
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_ETF_SUB_RED, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::FullSingle);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Ack);
        assert_eq!(u32::from_be_bytes(plans[0].frame[0..4].try_into().unwrap()), 201_202);
    }

    #[test]
    fn test_cash_business_still_allows_partial() {
        // 竞价（010）部分成交策略不受影响
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_CASH, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::PartialSingle);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 2);
        let trade = ExecRptTrade::decode(
            msg_type::EXEC_RPT_CASH_TRADE,
            &plans[1].frame[8..plans[1].frame.len() - 4],
        )
        .unwrap();
        assert!(trade.last_qty < 1000 * QTY_UNIT);
        assert_eq!(trade.ord_status, ord_status::PARTIALLY_FILLED);
    }

    #[test]
    fn test_hk_ack_uses_hk_msg_type() {
        // 港股通（630）：确认走 206302
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_HK_CONNECT, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::AckOnly);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 1);
        assert_eq!(u32::from_be_bytes(plans[0].frame[0..4].try_into().unwrap()), 206_302);
    }

    #[test]
    fn test_full_split_sums_to_total() {
        for _ in 0..50 {
            let qtys = split_shares(1000, 3);
            assert_eq!(qtys.len(), 3);
            assert_eq!(qtys.iter().sum::<i64>(), 1000);
            assert!(qtys.iter().all(|&q| q >= 100));
        }
    }

    #[test]
    fn test_partial_less_than_total() {
        for _ in 0..50 {
            let p = partial_shares(1000);
            assert!(p >= 100 && p < 1000);
        }
    }

    #[test]
    fn test_ladder_prices_buy_ascend_to_limit() {
        let st = mk_cfg(StrategyMode::FullSplit).strategy;
        let order = mk_order(msg_type::NEW_ORDER_CASH, 300, 10.0, b'1');
        let fills = with_ladder_prices(&st, &order, &[100, 100, 100]);
        assert_eq!(fills[0].1, px_raw(9.98));
        assert_eq!(fills[1].1, px_raw(9.99));
        assert_eq!(fills[2].1, px_raw(10.0));
    }

    #[test]
    fn test_ladder_prices_sell_descend_to_limit() {
        let st = mk_cfg(StrategyMode::FullSplit).strategy;
        let order = mk_order(msg_type::NEW_ORDER_CASH, 300, 10.0, b'2');
        let fills = with_ladder_prices(&st, &order, &[100, 100, 100]);
        assert_eq!(fills[0].1, px_raw(10.02));
        assert_eq!(fills[2].1, px_raw(10.0));
    }

    #[test]
    fn test_reject_only_one_report() {
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_CASH, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::Reject);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Reject);
    }

    #[test]
    fn test_ack_only() {
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_CASH, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::AckOnly);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Ack);
    }

    #[test]
    fn test_custom_fills() {
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_CASH, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::Custom);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 3); // ack + 2 笔自定义成交
    }

    #[test]
    fn test_custom_fills_clamped_to_order_qty() {
        // 明细总量（300+200=500）超过委托量（400）时，逐笔钳制到剩余量：
        // 第二笔 200 被截为 100，累计成交量不超过委托量（协议非法场景防护）
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_CASH, 400, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::Custom);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 3); // ack + 2 笔
        let t1 = ExecRptTrade::decode(
            msg_type::EXEC_RPT_CASH_TRADE,
            &plans[1].frame[8..plans[1].frame.len() - 4],
        )
        .unwrap();
        assert_eq!(t1.last_qty, 300 * QTY_UNIT);
        let t2 = ExecRptTrade::decode(
            msg_type::EXEC_RPT_CASH_TRADE,
            &plans[2].frame[8..plans[2].frame.len() - 4],
        )
        .unwrap();
        assert_eq!(t2.last_qty, 100 * QTY_UNIT); // 200 被截为剩余 100
        assert_eq!(t2.cum_qty, 400 * QTY_UNIT); // 累计不超委托量
        assert_eq!(t2.ord_status, ord_status::FILLED);
    }

    #[test]
    fn test_custom_fills_downgraded_outside_cash() {
        // 非竞价业务 + Custom：降级为全部成交单笔
        let stats = PlatformStats::default();
        let order = mk_order(msg_type::NEW_ORDER_BOND_REPO, 1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::Custom);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 2);
        let trade = ExecRptTrade::decode(
            msg_type::EXEC_RPT_BOND_REPO_TRADE,
            &plans[1].frame[8..plans[1].frame.len() - 4],
        )
        .unwrap();
        assert_eq!(trade.last_qty, 1000 * QTY_UNIT);
    }

    #[test]
    fn test_biz_info_mapping_covers_all_order_types() {
        // 抽查关键业务特征：确认/成交报文类型、部分成交开关
        let b = biz_info(msg_type::NEW_ORDER_OPTION_AUCTION);
        assert_eq!(b.ack_msg_type, 200_402);
        assert_eq!(b.trade_msg_type, Some(200_415));
        assert!(!b.allow_partial);
        let b = biz_info(msg_type::NEW_ORDER_DISPOSAL);
        assert_eq!(b.ack_msg_type, 202_702);
        assert_eq!(b.trade_msg_type, None); // 转处置无成交回报
        let b = biz_info(msg_type::NEW_ORDER_BOND_BID);
        assert_eq!(b.ack_msg_type, 204_129);
        assert_eq!(b.trade_msg_type, Some(204_130));
        let b = biz_info(msg_type::NEW_ORDER_CASH);
        assert!(b.allow_partial);
        // 三方回购（表 3-3 补充的 28 种消息之一）：无扩展字段、无成交回报
        let b = biz_info(msg_type::NEW_ORDER_3P_REPO);
        assert_eq!(b.appl_id, "330");
        assert_eq!(b.ack_msg_type, 203_302);
        assert_eq!(b.trade_msg_type, None);
        assert!(!b.allow_partial);
    }

    #[test]
    fn test_biz_info_by_appl_id_mapping() {
        // 撤单/手动回报按 ApplID 反查业务：单值业务直查
        let b = biz_info_by_appl_id("010").unwrap();
        assert_eq!(b.ack_msg_type, 200_102);
        assert!(b.allow_partial); // 现货竞价唯一支持部分成交
        // 同一消息类型的多 ApplID 合并（表 3-3 第三位为申报代码）
        assert_eq!(
            biz_info_by_appl_id("051").unwrap().ack_msg_type,
            biz_info_by_appl_id("052").unwrap().ack_msg_type
        );
        assert_eq!(
            biz_info_by_appl_id("330").unwrap().ack_msg_type,
            biz_info_by_appl_id("331").unwrap().ack_msg_type
        );
        assert_eq!(
            biz_info_by_appl_id("130").unwrap().ack_msg_type,
            biz_info_by_appl_id("132").unwrap().ack_msg_type
        );
        // 未知 ApplID 返回 None（调用方兜底）
        assert!(biz_info_by_appl_id("999").is_none());
        assert!(biz_info_by_appl_id("").is_none());
    }
}
