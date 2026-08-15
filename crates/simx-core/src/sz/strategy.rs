//! 深交所模拟回报策略：根据配置为一笔委托生成回报计划（确认 + 成交 / 拒单）
//!
//! # 职责
//!
//! 模拟器不做真实撮合，而是按平台配置的策略生成回报计划：输入
//! 一笔委托 + 策略配置，输出一串 PlannedReport（每条含发送前等待
//! 时长与报文内容），由 session 模块按计划发送。
//!
//! 七种策略与对应剧本：
//! - FullSingle  全部成交（单笔）：1 确认 + 1 成交
//! - FullSplit   全部成交（拆单）：1 确认 + N 成交（数量随机拆、价格阶梯）
//! - PartialSingle 部分成交（单笔）：1 确认 + 1 成交（数量小于委托量）
//! - PartialSplit  部分成交（拆单）：1 确认 + N 成交（总量小于委托量）
//! - Custom      自定义：1 确认 + 用户逐笔指定的成交（数量/价格）
//! - AckOnly     只确认不成交：1 确认（挂单状态）
//! - Reject      拒单：1 条拒绝（可选拒单回报 200102 或业务拒绝 4）

use crate::config::{RejectVia, StrategyConfig, StrategyMode};
use crate::orderbook::{OrderStatus, OrderUpdate};
use super::protocol::{
    self as protocol, exec_type, msg_type, ord_status, BusinessReject, ExecRptCashAck,
    ExecRptCashTrade, NewOrderCash,
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
    /// 拒单（以执行回报 200102 形式）
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

/// 把“元”换算成协议的放大整数（如 10.01 元 → 100100）
fn px_raw(price_yuan: f64) -> i64 {
    (price_yuan * PX_UNIT).round() as i64
}

/// 把“股数”换算成协议的放大整数（如 100 股 → 10000）
fn qty_raw(shares: i64) -> i64 {
    shares * QTY_UNIT
}

/// 为一笔现货竞价委托生成回报计划（本模块唯一对外入口）。
///
/// 总体逻辑：
/// 1. 拒单策略 → 只生成 1 条拒绝，直接返回
/// 2. 其他策略 → 先生成 1 条确认，再由 gen_fills 算出成交明细，
///    逐笔生成成交回报（累计成交量/剩余量/订单状态逐步推进）
///
/// st 为策略配置（可来自运行时热更新的共享区）、partition_no 为平台分区号，
/// 两条回报的延迟由 st 的 ack_delay / trade_delay 抽样得到。
pub fn plan_reports(
    st: &StrategyConfig,
    partition_no: i32,
    order: &NewOrderCash,
    stats: &PlatformStats,
) -> Vec<PlannedReport> {
    let order_id = stats.next_order_id();
    let mut plans = Vec::new();

    // ---- 拒单策略：只回一条拒绝，没有确认也没有成交 ----
    if st.mode == StrategyMode::Reject {
        match st.reject_via {
            // 方式一：用执行回报(200102)拒单，在确认报文基础上
            // 把执行类型/订单状态改成“已拒绝”并带上拒绝原因代码
            RejectVia::ExecutionReport => {
                let mut rpt = base_ack(partition_no, order, stats, &order_id);
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
                        "拒单回报(200102) ClOrdID={} 原因代码={}",
                        order.cl_ord_id, st.reject_reason
                    ),
                    cl_ord_id: order.cl_ord_id.clone(),
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
                    appl_id: order.appl_id.clone(),
                    transact_time: protocol::now_timestamp(),
                    submitting_pbu_id: order.submitting_pbu_id.clone(),
                    security_id: order.security_id.clone(),
                    security_id_source: order.security_id_source.clone(),
                    ref_seq_num: 0,
                    ref_msg_type: msg_type::NEW_ORDER_CASH,
                    business_reject_ref_id: order.cl_ord_id.clone(),
                    business_reject_reason: st.reject_reason,
                    business_reject_text: st.reject_text.clone(),
                };
                plans.push(PlannedReport {
                    delay_ms: st.ack_delay.sample(),
                    kind: ReportKind::BusinessReject,
                    frame: rej.encode(),
                    desc: format!(
                        "业务拒绝(4) ClOrdID={} 原因={}",
                        order.cl_ord_id, st.reject_text
                    ),
                    cl_ord_id: order.cl_ord_id.clone(),
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
    let ack = base_ack(partition_no, order, stats, &order_id);
    plans.push(PlannedReport {
        delay_ms: st.ack_delay.sample(),
        kind: ReportKind::Ack,
        frame: ack.encode(),
        desc: format!(
            "确认回报(200102) ClOrdID={} OrderID={}",
            order.cl_ord_id,
            order_id.trim_start_matches('0')
        ),
        cl_ord_id: order.cl_ord_id.clone(),
        order_update: Some(OrderUpdate {
            order_id: order_id.clone(),
            cum_qty: 0.0,
            leaves_qty: order.order_qty as f64 / QTY_UNIT as f64,
            status: OrderStatus::New,
        }),
    });

    // ---- 成交回报：按策略算出的成交明细逐笔生成 ----
    let fills = gen_fills(st, order);
    let total: i64 = order.order_qty;
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
        let trade = ExecRptCashTrade {
            partition_no,
            report_index: stats.next_report_index(),
            appl_id: fill_or(&order.appl_id, "010"),
            reporting_pbu_id: order.submitting_pbu_id.clone(),
            submitting_pbu_id: order.submitting_pbu_id.clone(),
            security_id: order.security_id.clone(),
            security_id_source: fill_or(&order.security_id_source, "102"),
            owner_type: order.owner_type,
            clearing_firm: order.clearing_firm.clone(),
            transact_time: protocol::now_timestamp(),
            user_info: order.user_info.clone(),
            order_id: order_id.clone(),
            cl_ord_id: order.cl_ord_id.clone(),
            exec_id: stats.next_exec_id(),
            exec_type: exec_type::TRADE,
            ord_status: status,
            last_px: fill_px,
            last_qty: fill_qty,
            leaves_qty: leaves,
            cum_qty: cum,
            side: order.side,
            account_id: order.account_id.clone(),
            branch_id: order.branch_id.clone(),
            cash_margin: order.cash_margin,
        };
        plans.push(PlannedReport {
            delay_ms: st.trade_delay.sample(),
            kind: ReportKind::Trade,
            frame: trade.encode(),
            desc: format!(
                "成交回报(200115) ClOrdID={} 第{}/{}笔 价格={:.4} 数量={} 剩余={}",
                order.cl_ord_id,
                i + 1,
                n,
                fill_px as f64 / PX_UNIT,
                fill_qty / QTY_UNIT,
                leaves / QTY_UNIT
            ),
            cl_ord_id: order.cl_ord_id.clone(),
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

/// 小工具：v 为空时用默认值顶上（某些柜台不填可选字段）
fn fill_or(v: &str, default: &str) -> String {
    if v.is_empty() {
        default.to_string()
    } else {
        v.to_string()
    }
}

/// 构造确认回报（ExecType=0 已报）：大部分字段直接回填委托里的值，
/// 再填上交易所分配的订单号/执行号/回报序号。
/// 确认时尚未成交：剩余量 = 委托量，累计成交量 = 0。
fn base_ack(
    partition_no: i32,
    order: &NewOrderCash,
    stats: &PlatformStats,
    order_id: &str,
) -> ExecRptCashAck {
    ExecRptCashAck {
        partition_no,
        report_index: stats.next_report_index(),
        appl_id: fill_or(&order.appl_id, "010"),
        reporting_pbu_id: order.submitting_pbu_id.clone(),
        submitting_pbu_id: order.submitting_pbu_id.clone(),
        security_id: order.security_id.clone(),
        security_id_source: fill_or(&order.security_id_source, "102"),
        owner_type: order.owner_type,
        clearing_firm: order.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: order.user_info.clone(),
        order_id: order_id.to_string(),
        cl_ord_id: order.cl_ord_id.clone(),
        orig_cl_ord_id: String::new(),
        exec_id: stats.next_exec_id(),
        exec_type: exec_type::NEW,
        ord_status: ord_status::NEW,
        ord_rej_reason: 0,
        leaves_qty: order.order_qty,
        cum_qty: 0,
        side: order.side,
        ord_type: order.ord_type,
        order_qty: order.order_qty,
        price: order.price,
        account_id: order.account_id.clone(),
        branch_id: order.branch_id.clone(),
        order_restrictions: order.order_restrictions.clone(),
        stop_px: order.stop_px,
        min_qty: order.min_qty,
        max_price_levels: order.max_price_levels,
        time_in_force: order.time_in_force,
        cash_margin: order.cash_margin,
    }
}

/// 生成成交明细列表：每项为 (数量 raw, 价格 raw)，均为协议放大整数。
/// 这里只决定“成交几笔、每笔多少股、什么价”，不管报文细节。
fn gen_fills(st: &StrategyConfig, order: &NewOrderCash) -> Vec<(i64, i64)> {
    let total_shares = order.order_qty / QTY_UNIT; // 委托股数
    match st.mode {
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
        // 自定义：直接用用户在界面上填的逐笔数量/价格（跳过数量为 0 的行）
        StrategyMode::Custom => st
            .custom_fills
            .iter()
            .filter(|f| f.qty > 0.0)
            .map(|f| (qty_raw(f.qty.round() as i64), px_raw(f.price)))
            .collect(),
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
    order: &NewOrderCash,
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
    use crate::config::{CustomFill, DelayConfig, PlatformConfig};

    /// 造一笔测试委托（数量单位：股；价格单位：元）
    fn mk_order(qty_shares: i64, price: f64, side: u8) -> NewOrderCash {
        NewOrderCash {
            appl_id: "010".into(),
            submitting_pbu_id: "100001".into(),
            security_id: "000001".into(),
            security_id_source: "102".into(),
            cl_ord_id: "CL001".into(),
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
        let order = mk_order(1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::FullSingle);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
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
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Reject);
    }

    #[test]
    fn test_ack_only() {
        let stats = PlatformStats::default();
        let order = mk_order(1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::AckOnly);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Ack);
    }

    #[test]
    fn test_custom_fills() {
        let stats = PlatformStats::default();
        let order = mk_order(1000, 10.0, b'1');
        let cfg = mk_cfg(StrategyMode::Custom);
        let plans = plan_reports(&cfg.strategy, cfg.partition_no, &order, &stats);
        assert_eq!(plans.len(), 3); // ack + 2 笔自定义成交
    }
}
