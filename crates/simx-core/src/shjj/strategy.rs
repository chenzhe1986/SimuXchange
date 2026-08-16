//! 上交所竞价模拟回报策略：根据配置为一笔委托生成回报计划（确认 + 成交 / 拒单）
//!
//! # 职责
//!
//! 与 sz/strategy.rs 同构：输入一笔委托 + 平台“报单自动回报模式”配置，输出一串
//! PlannedReport（每条含发送前等待时长与报文内容），由 shjj::session
//! 按计划发送。区别仅在报文格式与数量/价格放大倍数：
//! - 确认/拒单/撤成用 ExecutionReport（MsgType=32）
//! - 成交用成交执行报告（MsgType=103）
//! - 业务拒绝方式对应申报拒绝 OrderReject（MsgType=204）
//! - 价格放大 10 万倍、数量放大 1000 倍（深交所是 1 万 / 100）
//!
//! # 多业务支持（表 3.2.1 业务类型表）
//!
//! 20 种业务共用同一套策略框架，通过 `biz_info` 业务特征表区分差异：
//! - **回报分区号 SetID**：现货竞价（100010）用登录分区号（1-6,20 多分区），
//!   其余业务固定 991（其他业务）/ 992（指定登记/指定撤销）
//! - **有无成交确认**：仅 100010 / 300020 / 300021 有成交回报，
//!   其余业务只回申报响应，成交类策略自动失效
//! - **是否支持部分成交**：仅现货竞价支持；其余业务把
//!   PartialSingle/PartialSplit 等部分成交策略降级为全部成交
//!
//! 七种策略与对应剧本：
//! - FullSingle  全部成交（单笔）：1 确认 + 1 成交
//! - FullSplit   全部成交（拆单）：1 确认 + N 成交（数量随机拆、价格阶梯）
//! - PartialSingle 部分成交（单笔）：1 确认 + 1 成交（数量小于委托量）
//! - PartialSplit  部分成交（拆单）：1 确认 + N 成交（总量小于委托量）
//! - NoAutoReply 不自动回复：订单登记缓存，等界面手动回复
//! - AckOnly     只确认不成交：1 确认（挂单状态）
//! - Reject      拒单：1 条拒绝（执行报告 32 ExecType=8 或申报拒绝 204）

use crate::config::{RejectVia, StrategyConfig, StrategyMode};
use crate::orderbook::{OrderStatus, OrderUpdate};
use super::protocol::{
    self as protocol, exec_type, ord_status, ExecRpt, NewOrder, OrderReject, TradeRpt,
    BIZ_ID_CASH_AUCTION, BIZ_ID_COLLATERAL_IN, BIZ_ID_COLLATERAL_OUT, BIZ_ID_DESIGNATION,
    BIZ_ID_DESIGNATION_CANCEL, BIZ_ID_FUND_CONVERT, BIZ_ID_FUND_DIVIDEND, BIZ_ID_FUND_RED,
    BIZ_ID_FUND_SUB, BIZ_ID_FUND_SUB_ISSUE, BIZ_ID_FUND_TRANSFER, BIZ_ID_ISSUE,
    BIZ_ID_PWD_SERVICE, BIZ_ID_REMAIN_TRANSFER, BIZ_ID_RETURN_TRANSFER, BIZ_ID_RIGHTS,
    BIZ_ID_RIGHTS_BOND, BIZ_ID_SEC_SRC_IN, BIZ_ID_SEC_SRC_OUT, BIZ_ID_TENDER_ACCEPT,
    BIZ_ID_TENDER_CANCEL, SET_ID_DESIGNATION, SET_ID_OTHER_BIZ,
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
    /// 拒单（以执行报告 32 ExecType=8 形式）
    Reject,
    /// 申报拒绝（以独立消息 MsgType=204 形式）
    BusinessReject,
    /// 撤单成功回报（仅手动回复产生；不占自动策略的统计计数）
    Cancel,
}

/// 一条待发送的回报
pub struct PlannedReport {
    /// 发送前等待的毫秒数（相对上一条回报）
    pub delay_ms: u64,
    pub kind: ReportKind,
    /// 完整报文（含消息头尾，MsgSeqNum 由 writer 发送时补）
    pub frame: Vec<u8>,
    /// 日志描述
    pub desc: String,
    /// 本条回报属于哪笔委托（订单缓存更新用）
    pub cl_ord_id: String,
    /// 本条回报发出后要同步给订单缓存的更新（None = 无需更新）
    pub order_update: Option<OrderUpdate>,
}

/// 业务特征（表 3.2.1 业务类型表）：决定回报分区号、是否可撤单、是否有成交回报等
#[derive(Debug, Clone, Copy)]
pub struct BizInfo {
    /// 业务名称（日志/展示用）
    pub name: &'static str,
    /// 业务标识 BizID（表 3.2.1）
    pub biz_id: u32,
    /// 回报分区号 SetID：现货竞价 0 = 用登录分区号（1-6,20 多分区），其余固定 991/992
    pub set_id: u32,
    /// 是否支持撤单（表 3.2.1 撤单列；300010 发行业务注 1 仅 ETF 认购可撤，
    /// 模拟器简化按不可撤处理）
    pub allow_cancel: bool,
    /// 是否有成交确认（表 3.2.1 成交确认列）：无成交确认的业务只回申报响应
    pub allow_trade: bool,
    /// 是否支持部分成交（仅现货竞价 100010 支持；其余业务部分成交类策略降级为全部成交）
    pub allow_partial: bool,
}

/// 业务特征表：按表 3.2.1 顺序映射 20 种业务。
///
/// 有成交确认的业务仅 3 种：100010 / 300020 / 300021；其余业务只回申报响应。
pub fn biz_info(biz_id: u32) -> BizInfo {
    match biz_id {
        // 股票现货竞价：多分区（SetID 用登录分区号）、可撤单、有成交确认、唯一支持部分成交
        BIZ_ID_CASH_AUCTION => BizInfo {
            name: "股票现货竞价", biz_id, set_id: 0, allow_cancel: true, allow_trade: true, allow_partial: true,
        },
        // 发行：注 1 仅 ETF 认购可撤单，其他不可撤（模拟器简化按不可撤处理）
        BIZ_ID_ISSUE => BizInfo {
            name: "发行", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: false, allow_trade: false, allow_partial: false,
        },
        // 配股/科创板配售：不支持撤单、有成交确认
        BIZ_ID_RIGHTS => BizInfo {
            name: "配股/科创板配售", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: false, allow_trade: true, allow_partial: false,
        },
        // 配转债：不支持撤单、有成交确认
        BIZ_ID_RIGHTS_BOND => BizInfo {
            name: "配转债", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: false, allow_trade: true, allow_partial: false,
        },
        // 要约预受/要约撤销：可撤单、无成交确认
        BIZ_ID_TENDER_ACCEPT => BizInfo {
            name: "要约预受", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_TENDER_CANCEL => BizInfo {
            name: "要约撤销", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        // 基金申购/赎回/认购：可撤单、无成交确认
        BIZ_ID_FUND_SUB => BizInfo {
            name: "基金申购", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_FUND_RED => BizInfo {
            name: "基金赎回", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_FUND_SUB_ISSUE => BizInfo {
            name: "基金认购", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        // 转托管/分红设置/转换：可撤单、无成交确认
        BIZ_ID_FUND_TRANSFER => BizInfo {
            name: "转托管", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_FUND_DIVIDEND => BizInfo {
            name: "分红设置", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_FUND_CONVERT => BizInfo {
            name: "转换", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        // 余券/还券/担保品/券源划转：可撤单、无成交确认
        BIZ_ID_REMAIN_TRANSFER => BizInfo {
            name: "余券划转", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_RETURN_TRANSFER => BizInfo {
            name: "还券划转", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_COLLATERAL_IN => BizInfo {
            name: "担保品划入", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_COLLATERAL_OUT => BizInfo {
            name: "担保品划出", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_SEC_SRC_IN => BizInfo {
            name: "券源划入", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_SEC_SRC_OUT => BizInfo {
            name: "券源划出", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: true, allow_trade: false, allow_partial: false,
        },
        // 网络密码服务：注 2 不重单校验、响应不进执行报告流（不经过本策略框架）
        BIZ_ID_PWD_SERVICE => BizInfo {
            name: "网络密码服务", biz_id, set_id: SET_ID_OTHER_BIZ, allow_cancel: false, allow_trade: false, allow_partial: false,
        },
        // 指定登记/指定撤销：SetID=992、不可撤单、无成交确认
        BIZ_ID_DESIGNATION => BizInfo {
            name: "指定登记", biz_id, set_id: SET_ID_DESIGNATION, allow_cancel: false, allow_trade: false, allow_partial: false,
        },
        BIZ_ID_DESIGNATION_CANCEL => BizInfo {
            name: "指定撤销", biz_id, set_id: SET_ID_DESIGNATION, allow_cancel: false, allow_trade: false, allow_partial: false,
        },
        // 未知业务：按现货竞价兜底（session 层会先拒绝未知 BizID，这里仅防御）
        _ => BizInfo {
            name: "未知业务", biz_id, set_id: 0, allow_cancel: true, allow_trade: true, allow_partial: true,
        },
    }
}

/// 是否为表 3.2.1 内的已知业务（未知 BizID 在 session 层回申报拒绝 204，错误码 4012）
pub fn is_known_biz(biz_id: u32) -> bool {
    matches!(
        biz_id,
        BIZ_ID_CASH_AUCTION
            | BIZ_ID_ISSUE
            | BIZ_ID_RIGHTS
            | BIZ_ID_RIGHTS_BOND
            | BIZ_ID_TENDER_ACCEPT
            | BIZ_ID_TENDER_CANCEL
            | BIZ_ID_FUND_SUB
            | BIZ_ID_FUND_RED
            | BIZ_ID_FUND_SUB_ISSUE
            | BIZ_ID_FUND_TRANSFER
            | BIZ_ID_FUND_DIVIDEND
            | BIZ_ID_FUND_CONVERT
            | BIZ_ID_REMAIN_TRANSFER
            | BIZ_ID_RETURN_TRANSFER
            | BIZ_ID_COLLATERAL_IN
            | BIZ_ID_COLLATERAL_OUT
            | BIZ_ID_SEC_SRC_IN
            | BIZ_ID_SEC_SRC_OUT
            | BIZ_ID_PWD_SERVICE
            | BIZ_ID_DESIGNATION
            | BIZ_ID_DESIGNATION_CANCEL
    )
}

/// Qty N15(3)：协议中数量放大 1000 倍存储，1 股 = 1000
const QTY_UNIT: i64 = 1000;
/// Price N13(5)：协议中价格放大 10 万倍存储，1 元 = 100000
const PX_UNIT: f64 = 100000.0;
/// 沪市股票一手 = 100 股（拆单时尽量按整手拆）
const LOT: i64 = 100;

/// 把“元”换算成协议的放大整数（如 10.01 元 → 1001000）
fn px_raw(price_yuan: f64) -> i64 {
    (price_yuan * PX_UNIT).round() as i64
}

/// 把“股数”换算成协议的放大整数（如 100 股 → 100000）
fn qty_raw(shares: i64) -> i64 {
    shares * QTY_UNIT
}

/// 成交金额 GrossTradeAmt = 价格 × 数量，两者都是放大整数：
/// N13(5) × N15(3) 直接相乘会多放大 1000 倍，除回去得到 N18(5)。
/// 用 i128 中间量防极端价格×数量相乘溢出 i64。
fn amount_raw(px: i64, qty: i64) -> i64 {
    ((px as i128) * (qty as i128) / QTY_UNIT as i128) as i64
}

/// 为一笔现货竞价委托生成回报计划（本模块唯一对外入口）。
///
/// pbu 为回报交易单元（取柜台 Logon 的 SenderCompID 前 8 字符），
/// 上交所回报里 Pbu/SetID/ReportIndex 三件套用于分区回报定位。
pub fn plan_reports(
    st: &StrategyConfig,
    partition_no: i32,
    order: &NewOrder,
    stats: &PlatformStats,
    pbu: &str,
) -> Vec<PlannedReport> {
    let biz = biz_info(order.biz_id);
    // 现货竞价用登录分区号（表 3.2.1 SetID 1-6,20 多分区），其余业务用表固定值 991/992
    let set_id = if biz.set_id == 0 { partition_no as u32 } else { biz.set_id };
    let ord_cnfm_id = stats.next_order_id();
    let mut plans = Vec::new();

    // ---- 不自动回复（挂单手动回复）：连确认都不自动回 ----
    // 订单已在会话层登记进缓存，等柜台在订单界面手动回复确认/成交/拒单/撤单
    if st.mode == StrategyMode::NoAutoReply {
        return Vec::new();
    }


    // ---- 拒单策略：只回一条拒绝，没有确认也没有成交 ----
    if st.mode == StrategyMode::Reject {
        match st.reject_via {
            // 方式一：用执行报告(32)拒单，ExecType/OrdStatus 均为 '8'
            RejectVia::ExecutionReport => {
                let mut rpt = base_ack(set_id, order, pbu, &ord_cnfm_id);
                rpt.exec_type = exec_type::REJECT;
                rpt.ord_status = ord_status::REJECTED;
                rpt.ord_rej_reason = st.reject_reason as u32;
                rpt.leaves_qty = 0;
                // 拒单不是撤单：CxlQty（撤单数量）必须为 0，
                // TimeInForce 也不回填委托值（拒单回报不涉及成交时效）
                rpt.cxl_qty = 0;
                rpt.time_in_force = 0;
                plans.push(PlannedReport {
                    delay_ms: st.ack_delay.sample(),
                    kind: ReportKind::Reject,
                    frame: rpt.encode(),
                    desc: format!(
                        "拒单回报(32) ClOrdID={} 原因代码={}",
                        order.cl_ord_id, st.reject_reason
                    ),
                    cl_ord_id: order.cl_ord_id.clone(),
                    order_update: Some(OrderUpdate {
                        order_id: ord_cnfm_id.clone(),
                        cum_qty: 0.0,
                        leaves_qty: 0.0,
                        status: OrderStatus::Rejected,
                    }),
                });
            }
            // 方式二：用独立的申报拒绝消息(MsgType=204)
            RejectVia::BusinessReject => {
                let rej = OrderReject {
                    biz_id: order.biz_id,
                    biz_pbu: order.biz_pbu.clone(),
                    cl_ord_id: order.cl_ord_id.clone(),
                    security_id: order.security_id.clone(),
                    ord_rej_reason: st.reject_reason as u32,
                    trade_date: protocol::now_date(),
                    transact_time: protocol::now_ntime(),
                    user_info: order.user_info.clone(),
                };
                plans.push(PlannedReport {
                    delay_ms: st.ack_delay.sample(),
                    kind: ReportKind::BusinessReject,
                    frame: rej.encode(),
                    desc: format!(
                        "申报拒绝(204) ClOrdID={} 原因代码={}",
                        order.cl_ord_id, st.reject_reason
                    ),
                    cl_ord_id: order.cl_ord_id.clone(),
                    order_update: Some(OrderUpdate {
                        order_id: String::new(), // 申报拒绝未分配订单确认编号
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
    let ack = base_ack(set_id, order, pbu, &ord_cnfm_id);
    plans.push(PlannedReport {
        delay_ms: st.ack_delay.sample(),
        kind: ReportKind::Ack,
        frame: ack.encode(),
        desc: format!(
            "申报响应(32) ClOrdID={} OrdCnfmID={}",
            order.cl_ord_id,
            ord_cnfm_id.trim_start_matches('0')
        ),
        cl_ord_id: order.cl_ord_id.clone(),
        order_update: Some(OrderUpdate {
            order_id: ord_cnfm_id.clone(),
            cum_qty: 0.0,
            leaves_qty: order.order_qty as f64 / QTY_UNIT as f64,
            status: OrderStatus::New,
        }),
    });

    // ---- 成交回报：仅表 3.2.1 有成交确认的业务生成（100010/300020/300021）。
    // 其余业务订单保持“已报”挂单状态，由柜台后续撤销或等待人工处理 ----
    if !biz.allow_trade {
        return plans;
    }
    let fills = gen_fills(st, order, &biz);
    let total: i64 = order.order_qty;
    let mut cum: i64 = 0; // 累计已成交数量
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
        let trade = TradeRpt {
            pbu: pbu.to_string(),
            set_id,
            report_index: 0, // 发送时由 writer 任务补写（保证与线上顺序一致）
            biz_id: order.biz_id,
            exec_type: exec_type::TRADE,
            biz_pbu: order.biz_pbu.clone(),
            cl_ord_id: order.cl_ord_id.clone(),
            security_id: order.security_id.clone(),
            account: order.account.clone(),
            owner_type: order.owner_type,
            order_entry_time: order.transact_time,
            last_px: fill_px,
            last_qty: fill_qty,
            gross_trade_amt: amount_raw(fill_px, fill_qty),
            side: order.side,
            order_qty: order.order_qty,
            leaves_qty: leaves,
            ord_status: status,
            credit_tag: order.credit_tag.clone(),
            clearing_firm: order.clearing_firm.clone(),
            branch_id: order.branch_id.clone(),
            trd_cnfm_id: stats.next_exec_id(),
            ord_cnfm_id: ord_cnfm_id.clone(),
            trade_date: protocol::now_date(),
            transact_time: protocol::now_ntime(),
            user_info: order.user_info.clone(),
        };
        plans.push(PlannedReport {
            delay_ms: st.trade_delay.sample(),
            kind: ReportKind::Trade,
            frame: trade.encode(),
            desc: format!(
                "成交回报(103) ClOrdID={} 第{}/{}笔 价格={:.5} 数量={} 剩余={}",
                order.cl_ord_id,
                i + 1,
                n,
                fill_px as f64 / PX_UNIT,
                fill_qty / QTY_UNIT,
                leaves / QTY_UNIT
            ),
            cl_ord_id: order.cl_ord_id.clone(),
            order_update: Some(OrderUpdate {
                order_id: ord_cnfm_id.clone(),
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

/// 构造申报响应（ExecType='0' 申报成功）：大部分字段直接回填委托里的值，
/// 再填上交易所分配的订单确认编号；回报序号 ReportIndex 由 writer 在发送时
/// 补写（保证与线上顺序一致）。
/// 确认时尚未成交：剩余量 = 委托量，已撤量 = 0。
fn base_ack(
    set_id: u32,
    order: &NewOrder,
    pbu: &str,
    ord_cnfm_id: &str,
) -> ExecRpt {
    ExecRpt {
        pbu: pbu.to_string(),
        set_id,
        report_index: 0, // 发送时由 writer 任务补写（保证与线上顺序一致）
        biz_id: order.biz_id,
        exec_type: exec_type::NEW,
        biz_pbu: order.biz_pbu.clone(),
        cl_ord_id: order.cl_ord_id.clone(),
        security_id: order.security_id.clone(),
        account: order.account.clone(),
        owner_type: order.owner_type,
        side: order.side,
        price: order.price,
        order_qty: order.order_qty,
        leaves_qty: order.order_qty,
        cxl_qty: 0,
        ord_type: order.ord_type,
        time_in_force: order.time_in_force,
        ord_status: ord_status::NEW,
        credit_tag: order.credit_tag.clone(),
        orig_cl_ord_id: String::new(),
        clearing_firm: order.clearing_firm.clone(),
        branch_id: order.branch_id.clone(),
        ord_rej_reason: 0,
        ord_cnfm_id: ord_cnfm_id.to_string(),
        orig_ord_cnfm_id: String::new(),
        trade_date: protocol::now_date(),
        transact_time: protocol::now_ntime(),
        user_info: order.user_info.clone(),
        // 4.3.3.1 说明 2：申报响应的扩展字段与新订单对应业务一致
        extend: order.extend.clone(),
    }
}

/// 生成成交明细列表：每项为 (数量 raw, 价格 raw)，均为协议放大整数。
/// 这里只决定“成交几笔、每笔多少股、什么价”，不管报文细节。
///
/// 差异点：除现货竞价（100010）外的业务不支持部分成交，把
/// PartialSingle/PartialSplit 降级为对应的全部成交策略。
fn gen_fills(st: &StrategyConfig, order: &NewOrder, biz: &BizInfo) -> Vec<(i64, i64)> {
    let total_shares = order.order_qty / QTY_UNIT; // 委托股数
    // 非现货业务的部分成交降级：PartialSingle→FullSingle、PartialSplit→FullSplit、
    let mode = if biz.allow_partial {
        st.mode
    } else {
        match st.mode {
            StrategyMode::PartialSingle => StrategyMode::FullSingle,
            StrategyMode::PartialSplit => StrategyMode::FullSplit,
            m => m,
        }
    };
    match mode {
        // 全部成交（单笔）：一笔成交全部数量，价格就是委托价
        StrategyMode::FullSingle => {
            vec![(order.order_qty, order.price)]
        }
        // 全部成交（拆单）：先随机拆成若干笔，再配阶梯价
        StrategyMode::FullSplit => {
            let qtys = split_shares(total_shares, sample_split_count(st));
            with_ladder_prices(st, order, &qtys)
        }
        // 部分成交（单笔）：随机取一个小于委托量的数量
        StrategyMode::PartialSingle => {
            let part = partial_shares(total_shares);
            vec![(qty_raw(part), order.price)]
        }
        // 部分成交（拆单）：先定部分成交总量，再拆单配阶梯价
        StrategyMode::PartialSplit => {
            let part = partial_shares(total_shares);
            let qtys = split_shares(part, sample_split_count(st));
            with_ladder_prices(st, order, &qtys)
        }
        // 只确认/拒单：没有成交
        // NoAutoReply 在 plan_reports 已早退（不生成任何计划），这里不可达
        StrategyMode::NoAutoReply | StrategyMode::AckOnly | StrategyMode::Reject => Vec::new(),
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
            let px = if order.side == b'2' {
                // 卖单：从高到低递减至委托价
                order.price + tick * steps
            } else {
                // 买单：从低到高递增至委托价
                (order.price - tick * steps).max(tick)
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
    use crate::config::{DelayConfig, PlatformConfig};
    use super::protocol::BIZ_ID_CASH_AUCTION;

    /// 造一笔测试委托（数量单位：股；价格单位：元）
    fn mk_order(qty_shares: i64, price: f64, side: u8) -> NewOrder {
        NewOrder {
            biz_id: BIZ_ID_CASH_AUCTION,
            biz_pbu: "PBU00001".into(),
            cl_ord_id: "A000000001".into(),
            security_id: "600000".into(),
            account: "B880000001".into(),
            side,
            ord_type: b'2',
            order_qty: qty_shares * QTY_UNIT,
            price: px_raw(price),
            ..Default::default()
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
        let order = mk_order(1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::FullSingle);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats, "PBU1");
        assert_eq!(plans.len(), 2); // ack + 1 笔成交
        assert_eq!(plans[0].kind, ReportKind::Ack);
        assert_eq!(plans[1].kind, ReportKind::Trade);
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
        let order = mk_order(300, 10.0, b'1');
        let fills = with_ladder_prices(&st, &order, &[100, 100, 100]);
        assert_eq!(fills[0].1, px_raw(9.98));
        assert_eq!(fills[1].1, px_raw(9.99));
        assert_eq!(fills[2].1, px_raw(10.0));
    }

    #[test]
    fn test_ladder_prices_sell_descend_to_limit() {
        let st = mk_cfg(StrategyMode::FullSplit).strategy;
        let order = mk_order(300, 10.0, b'2');
        let fills = with_ladder_prices(&st, &order, &[100, 100, 100]);
        assert_eq!(fills[0].1, px_raw(10.02));
        assert_eq!(fills[2].1, px_raw(10.0));
    }

    #[test]
    fn test_reject_only_one_report() {
        let stats = PlatformStats::default();
        let order = mk_order(1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::Reject);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats, "PBU1");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Reject);
    }

    #[test]
    fn test_ack_only() {
        let stats = PlatformStats::default();
        let order = mk_order(1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::AckOnly);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats, "PBU1");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Ack);
    }

    #[test]
    fn test_amount_raw() {
        // 10 元 × 100 股 = 1000 元 → N18(5) 表示为 100000000
        assert_eq!(amount_raw(px_raw(10.0), qty_raw(100)), 100_000_000);
    }

    #[test]
    fn test_other_biz_set_id_fixed() {
        // 非现货业务（300030 要约预受）：SetID 固定 991，不受登录分区号影响；
        // 且无成交确认（表 3.2.1），FullSingle 也只回一条申报响应
        let stats = PlatformStats::default();
        let mut order = mk_order(1000, 10.0, b'1');
        order.biz_id = protocol::BIZ_ID_TENDER_ACCEPT;
        let cfg = mk_cfg(StrategyMode::FullSingle);
        let plans = plan_reports(&cfg.strategy, 3, &order, &stats, "PBU1");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Ack);
        let body = &plans[0].frame[16..plans[0].frame.len() - 4];
        let mut r = protocol::BodyReader::new(body);
        r.str(8).unwrap(); // PBU
        assert_eq!(r.u32().unwrap(), SET_ID_OTHER_BIZ);
        // 现货竞价仍用登录分区号（多分区 1-6,20）
        let order2 = mk_order(1000, 10.0, b'1');
        let plans2 = plan_reports(&cfg.strategy, 5, &order2, &stats, "PBU1");
        let body2 = &plans2[0].frame[16..plans2[0].frame.len() - 4];
        let mut r2 = protocol::BodyReader::new(body2);
        r2.str(8).unwrap();
        assert_eq!(r2.u32().unwrap(), 5);
    }

    #[test]
    fn test_gen_fills_degrade_for_other_biz() {
        // 300020 配股/科创板配售：不支持部分成交，PartialSingle 降级为 FullSingle
        let st = mk_cfg(StrategyMode::PartialSingle).strategy;
        let mut order = mk_order(1000, 10.0, b'1');
        order.biz_id = protocol::BIZ_ID_RIGHTS;
        let fills = gen_fills(&st, &order, &biz_info(order.biz_id));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].0, order.order_qty); // 一笔全量成交
        // 现货竞价保持部分成交语义（数量小于全量）
        let order2 = mk_order(1000, 10.0, b'1');
        let fills2 = gen_fills(&st, &order2, &biz_info(order2.biz_id));
        assert_eq!(fills2.len(), 1);
        assert!(fills2[0].0 < order2.order_qty);
    }

    #[test]
    fn test_biz_info_flags() {
        // 现货：可撤、有成交、支持部分成交
        let cash = biz_info(protocol::BIZ_ID_CASH_AUCTION);
        assert!(cash.allow_cancel && cash.allow_trade && cash.allow_partial);
        // 配股/配转债：有成交、不可撤、不支持部分成交
        assert!(!biz_info(protocol::BIZ_ID_RIGHTS).allow_cancel);
        assert!(biz_info(protocol::BIZ_ID_RIGHTS).allow_trade);
        assert!(!biz_info(protocol::BIZ_ID_RIGHTS).allow_partial);
        assert!(biz_info(protocol::BIZ_ID_RIGHTS_BOND).allow_trade);
        // 要约预受：可撤、无成交
        assert!(biz_info(protocol::BIZ_ID_TENDER_ACCEPT).allow_cancel);
        assert!(!biz_info(protocol::BIZ_ID_TENDER_ACCEPT).allow_trade);
        // 指定登记：SetID=992；其余非现货业务固定 991
        assert_eq!(biz_info(protocol::BIZ_ID_DESIGNATION).set_id, SET_ID_DESIGNATION);
        assert_eq!(biz_info(protocol::BIZ_ID_DESIGNATION_CANCEL).set_id, SET_ID_DESIGNATION);
        assert_eq!(biz_info(protocol::BIZ_ID_FUND_SUB).set_id, SET_ID_OTHER_BIZ);
        assert_eq!(biz_info(protocol::BIZ_ID_FUND_TRANSFER).set_id, SET_ID_OTHER_BIZ);
        // 带扩展字段的业务（转托管/分红设置/转换）可撤单
        assert!(biz_info(protocol::BIZ_ID_FUND_TRANSFER).allow_cancel);
        assert!(biz_info(protocol::BIZ_ID_FUND_DIVIDEND).allow_cancel);
        assert!(biz_info(protocol::BIZ_ID_FUND_CONVERT).allow_cancel);
    }
}
