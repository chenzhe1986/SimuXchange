//! 上海综合平台模拟回报策略：把一笔 STEP 委托变成一串待写库的回报。
//!
//! # 与 TCP 网关的区别
//!
//! 这里没有“发送报文”这个动作，而是生成一条条**待执行的数据库写入**：
//! - 确认/拒单/撤单响应 → 回写 reqresp 同一行（resptext + localrespnum）；
//! - 成交回报 → 向 execreport 插入一行（execreporttext）。
//!
//! 因此 [`PlannedReport`] 不携带二进制帧，而是 STEP 文本 + 回报种类；
//! 由 session 模块负责按延迟执行并调用 db 模块。
//!
//! 策略配置复用 config.rs 的 [`StrategyConfig`]（与三套 TCP 协议完全一致）：
//! 全部成交/部分成交/随机拆多笔/只回确认/自动拒单/Lua 脚本/关闭自动回报，
//! 撤单也复用 CancelMode。差异只有两点：
//! - 综合平台没有独立的“业务拒绝消息”，RejectVia::BusinessReject 降级为
//!   执行报告拒绝（ExecType=8）；
//! - 各业务规范上不一定有成交回报（见 protocol::biz_mapping），无成交回报
//!   的业务即使配置“全部成交”也只回确认。

use crate::config::{CancelMode, StrategyConfig, StrategyMode};
use crate::lua_engine::{ScriptEnv, ScriptKind};
use crate::orderbook::{OrderEntry, OrderStatus, OrderUpdate};
use crate::pcf::PcfFile;
use crate::stats::PlatformStats;
use rand::Rng;
use std::collections::BTreeMap;

use super::protocol::{self as step, biz_mapping, Party, StepOrder};

/// 一条待执行的回报（写库动作）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// 确认回报（回写 reqresp）
    Ack,
    /// 成交回报（插入 execreport）
    Trade,
    /// 拒单（回写 reqresp，ExecType=8）
    Reject,
    /// 撤单成功（回写 reqresp，ExecType=6）
    Cancel,
}

/// 一条待执行的回报
#[derive(Debug, Clone)]
pub struct PlannedReport {
    /// 相对上一条回报的等待毫秒数（0 = 立即执行）
    pub delay_ms: u64,
    pub kind: ReportKind,
    /// STEP 文本（写进 resptext 或 execreporttext）
    pub text: String,
    /// 日志描述
    pub desc: String,
    /// 本条回报写入后同步订单缓存的更新
    pub order_update: Option<OrderUpdate>,
    /// 成交回报在 execreport.seqnum 里的序号（同一订单第几笔；非成交为 0）
    pub trade_seq: i32,
}

/// 按策略配置生成一笔**新委托**的回报计划。
///
/// `env` 为 Lua 脚本模式的执行环境（非 Lua 模式不使用，但统一携带）；
/// `pcf` 为上海 ETF 申赎清单解析结果（EEC/EER/IEC/IER 用，其他业务/未配置时为 None）。
/// 返回值按执行顺序排列；交付方只需逐条 sleep(delay_ms) 后写库。
pub fn plan_reports(
    st: &StrategyConfig,
    order: &StepOrder,
    stats: &PlatformStats,
    env: &ScriptEnv,
    pcf: Option<&PcfFile>,
) -> Vec<PlannedReport> {
    if st.mode == StrategyMode::NoAutoReply {
        return Vec::new();
    }
    if st.mode == StrategyMode::LuaScript {
        return plan_reports_lua(st, order, stats, env);
    }

    let mapping = biz_mapping(&order.reqid);

    // ---- 自动拒单：只回一条执行报告拒绝 ----
    if st.mode == StrategyMode::Reject {
        return build_reject_plans(st, order, stats, &st.reject_text);
    }

    let order_id = stats.next_order_id();

    // ---- 确认回报：拒单以外的策略都先回确认 ----
    let mut plans = vec![PlannedReport {
        delay_ms: st.ack_delay.sample(),
        kind: ReportKind::Ack,
        text: step::build_ack(order, &order_id, &step::now_timestamp()),
        desc: format!(
            "确认回报 ClOrdID={} OrderID={} 数量={}",
            order.cl_ord_id,
            order_id.trim_start_matches('0'),
            order.qty
        ),
        order_update: Some(OrderUpdate {
            order_id: order_id.clone(),
            cum_qty: 0.0,
            leaves_qty: order.qty,
            status: OrderStatus::New,
        }),
        trade_seq: 0,
    }];

    // 只回确认 / 规范上无成交回报的业务：到此为止
    if st.mode == StrategyMode::AckOnly || !mapping.has_exec_report || order.qty <= 0.0 {
        return plans;
    }

    // ---- ETF 申赎：按 PCF 生成 1.51 规范的成交回报（类型1/2/3） ----
    // 现金申赎（EEC/EER）只有 ETF 成交 + 资金替代两条；实物申赎（IEC/IER）
    // 还有逐只成份券成交。ETF 不支持部分成交/拆单，统一按整笔成交处理。
    if let Some(kind) = etf_kind(&order.reqid) {
        if matches!(
            st.mode,
            StrategyMode::PartialSingle | StrategyMode::PartialSplit | StrategyMode::FullSplit
        ) {
            (env.log)(
                "info",
                format!(
                    "ETF 申赎（{}）不支持部分成交/拆单，按整笔成交生成执行报告",
                    order.reqid
                ),
            );
        }
        plans.extend(plan_etf_records(st, order, stats, pcf, kind, &order_id, env));
        return plans;
    }

    // ---- 成交回报：按策略生成成交明细 ----
    let fills = gen_fills(st, order);
    let mut cum = 0.0f64;
    let total = order.qty;
    let n = fills.len();
    for (i, (fill_qty, fill_px)) in fills.into_iter().enumerate() {
        cum += fill_qty;
        let leaves = (total - cum).max(0.0);
        let filled = leaves <= 0.000001;
        let exec_id = stats.next_exec_id();
        let text = step::build_trade(
            order,
            &order_id,
            &exec_id,
            fill_px,
            fill_qty,
            leaves,
            filled,
            &step::now_timestamp(),
        );
        plans.push(PlannedReport {
            delay_ms: st.trade_delay.sample(),
            kind: ReportKind::Trade,
            text,
            desc: format!(
                "成交回报 ClOrdID={} 第{}/{}笔 价格={:.3} 数量={} 剩余={}",
                order.cl_ord_id,
                i + 1,
                n,
                fill_px,
                fill_qty,
                leaves
            ),
            order_update: Some(OrderUpdate {
                order_id: order_id.clone(),
                cum_qty: cum,
                leaves_qty: leaves,
                status: if filled {
                    OrderStatus::Filled
                } else {
                    OrderStatus::Partial
                },
            }),
            trade_seq: (i + 1) as i32,
        });
    }
    plans
}

/// 业务规则强制拒单（当前用于 ETF 申购成份股持仓不足）：
/// 综合平台没有独立的业务拒绝消息，统一用执行报告拒绝（ExecType=8），
/// 原因码取平台配置，`reason_text` 写进 Text 字段说明具体原因。
pub fn plan_force_reject(
    st: &StrategyConfig,
    order: &StepOrder,
    stats: &PlatformStats,
    reason_text: &str,
) -> Vec<PlannedReport> {
    build_reject_plans(st, order, stats, reason_text)
}

/// 生成“只回一条执行报告拒绝”的计划（自动拒单模式与业务规则强制拒单共用）。
fn build_reject_plans(
    st: &StrategyConfig,
    order: &StepOrder,
    stats: &PlatformStats,
    text: &str,
) -> Vec<PlannedReport> {
    let order_id = stats.next_order_id();
    let step_text = step::build_reject(
        order,
        &order_id,
        st.reject_reason as i64,
        text,
        &step::now_timestamp(),
    );
    vec![PlannedReport {
        delay_ms: st.ack_delay.sample(),
        kind: ReportKind::Reject,
        text: step_text,
        desc: format!(
            "拒单回报 ClOrdID={} 原因代码={} 说明={}（{}）",
            order.cl_ord_id,
            st.reject_reason,
            text,
            if st.reject_via == crate::config::RejectVia::BusinessReject {
                "综合平台无业务拒绝消息，按执行报告拒绝发送"
            } else {
                "执行报告拒绝"
            }
        ),
        order_update: Some(OrderUpdate {
            order_id,
            cum_qty: 0.0,
            leaves_qty: 0.0,
            status: OrderStatus::Rejected,
        }),
        trade_seq: 0,
    }]
}

/// ETF 申赎业务类型（决定成交回报结构）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EtfKind {
    /// ETF 现金申购（EEC）
    CashCreation,
    /// ETF 现金赎回（EER）
    CashRedemption,
    /// ETF 实物申购（IEC）
    PhysicalCreation,
    /// ETF 实物赎回（IER）
    PhysicalRedemption,
}

impl EtfKind {
    fn is_redeem(self) -> bool {
        matches!(self, Self::CashRedemption | Self::PhysicalRedemption)
    }
    fn is_physical(self) -> bool {
        matches!(self, Self::PhysicalCreation | Self::PhysicalRedemption)
    }
}

/// 按 reqid 判断是否 ETF 申赎业务（仅 EEC/EER/IEC/IER；黄金 ETF 等不在此列）
fn etf_kind(reqid: &str) -> Option<EtfKind> {
    match reqid.trim().to_ascii_uppercase().as_str() {
        "EEC" => Some(EtfKind::CashCreation),
        "EER" => Some(EtfKind::CashRedemption),
        "IEC" => Some(EtfKind::PhysicalCreation),
        "IER" => Some(EtfKind::PhysicalRedemption),
        _ => None,
    }
}

/// 生成 ETF 申赎的成交回报计划（1.51 规范 14.1.4 / 14.2.4）。
///
/// - 现金申赎：类型1（ETF 成交）+ 类型3（资金替代成交，资金类型固定 9）；
/// - 实物申赎：类型1 + 逐只成份券类型2（数量 = PCF Quantity × 篮子数，
///   买卖方向与 ETF 相反）+ 类型3（有现金替代时）。
///
/// 现金替代总额按规范计算：每个成份券的 SubstitutionCashAmount 按替代标志
/// 处理（1=允许现金，乘 1±溢价/折价；2=必须现金、3=退补现金，直接取；
/// 0=禁止现金，不计），单只先四舍五入到 3 位，汇总到 3 位，再乘篮子数、
/// 最终四舍五入到 2 位。申购且已同步持仓时，调用方传入的清单已经按
/// “允许现金替代优先扣持仓”调整过（见 positions::PositionStore），
/// 本函数无需感知持仓。
/// PCF 未配置/未找到时按 0 现金替代、0 只成份券生成结构完整的回报并告警。
fn plan_etf_records(
    st: &StrategyConfig,
    order: &StepOrder,
    stats: &PlatformStats,
    pcf: Option<&PcfFile>,
    kind: EtfKind,
    order_id: &str,
    env: &ScriptEnv,
) -> Vec<PlannedReport> {
    let is_redeem = kind.is_redeem();
    let is_physical = kind.is_physical();
    let etf_side = if is_redeem { '2' } else { '1' };
    let baskets = match pcf {
        Some(p) if p.creation_redemption_unit > 0.0 => {
            order.qty / p.creation_redemption_unit
        }
        _ => 0.0,
    };

    let mut components: Vec<(String, f64)> = Vec::new(); // (成份券代码, 过户数量)
    let mut per_basket_total = 0.0f64;
    let mut groups: BTreeMap<i32, f64> = BTreeMap::new();
    let mut flag0_count = 0usize;
    if let Some(pcf) = pcf {
        if baskets <= 0.0 {
            (env.log)(
                "warn",
                format!(
                    "上海 ETF 清单 [{}] 的最小申赎单位无效（{}），本笔按 0 只成份券/0 现金替代回报",
                    pcf.security_id, pcf.creation_redemption_unit
                ),
            );
        }
        for c in &pcf.components {
            // 实物申赎：只有禁止/允许现金替代（0/1）的成份券需要过户；
            // 必须现金替代（2）与退补现金替代（3）只出现在资金替代组中
            if is_physical
                && matches!(c.substitute_flag, 0 | 1)
                && c.component_share > 0.0
                && baskets > 0.0
            {
                components.push((
                    c.security_id.clone(),
                    (c.component_share * baskets).round(),
                ));
            }
            if c.substitute_flag == 0 && c.component_share > 0.0 {
                flag0_count += 1;
            }
            let amount = match c.substitute_flag {
                // 允许现金：申购加溢价、赎回减折价（比例字段是小数，不是百分比）
                1 => {
                    let rate = if is_redeem {
                        c.redemption_discount_rate
                    } else {
                        c.creation_premium_rate
                    };
                    let base = if is_redeem {
                        c.redemption_cash_substitute
                    } else {
                        c.creation_cash_substitute
                    };
                    round3(if is_redeem {
                        base * (1.0 - rate)
                    } else {
                        base * (1.0 + rate)
                    })
                }
                // 必须现金 / 退补现金：直接取替代金额（PCF 里申赎共用一个字段）
                2 | 3 => round3(c.creation_cash_substitute),
                // 禁止现金：不进资金替代
                _ => 0.0,
            };
            if amount != 0.0 {
                per_basket_total += amount;
                // 现金申赎按规范统一用资金类型 9；实物申赎按成分券市场分组
                let amt_type = if is_physical {
                    market_amt_type(&c.security_id_source)
                } else {
                    9
                };
                *groups.entry(amt_type).or_insert(0.0) += amount;
            }
        }
    }
    if flag0_count > 0 && !is_physical {
        (env.log)(
            "warn",
            format!(
                "现金申赎（{}）的清单里有 {} 只成份券禁止现金替代，已从现金替代总额中剔除",
                order.reqid, flag0_count
            ),
        );
    }

    // 每个资金分组：先按每篮子汇总四舍五入到 3 位，再乘篮子数、最终到 2 位
    let mut cash_groups: Vec<(i32, f64)> = groups
        .into_iter()
        .map(|(t, per_basket)| (t, round2(round3(per_basket) * baskets)))
        .filter(|(_, amount)| *amount != 0.0)
        .collect();
    let cash_total = round2(round3(per_basket_total) * baskets);
    // 现金申赎规范固定有两条记录（即使替代为 0 也保留资金替代组 9）
    if !is_physical && cash_groups.is_empty() {
        cash_groups.push((9, cash_total));
    }
    let include_cash = !is_physical || !cash_groups.is_empty();
    let num_trades = 1 + components.len() as u32 + if include_cash { 1 } else { 0 };

    let mut plans = Vec::new();
    let mut seq = 0i32;

    // 类型1：ETF 成交（数量=申赎份额，方向与业务一致）
    seq += 1;
    let exec_id = stats.next_exec_id();
    plans.push(PlannedReport {
        delay_ms: st.trade_delay.sample(),
        kind: ReportKind::Trade,
        text: step::build_etf_trade(
            order,
            order_id,
            &exec_id,
            '1',
            &order.security_id,
            order.qty,
            etf_side,
            0.0,
            &[],
            num_trades,
            &step::now_timestamp(),
        ),
        desc: format!(
            "ETF成交回报(类型1) ClOrdID={} {} 数量={}",
            order.cl_ord_id,
            if is_redeem { "赎回" } else { "申购" },
            order.qty
        ),
        order_update: Some(OrderUpdate {
            order_id: order_id.to_string(),
            cum_qty: order.qty,
            leaves_qty: 0.0,
            status: OrderStatus::Filled,
        }),
        trade_seq: seq,
    });

    // 类型2：逐只成份券过户（仅实物申赎）
    let component_side = if is_redeem { '1' } else { '2' };
    for (code, qty) in &components {
        seq += 1;
        let exec_id = stats.next_exec_id();
        plans.push(PlannedReport {
            delay_ms: st.trade_delay.sample(),
            kind: ReportKind::Trade,
            text: step::build_etf_trade(
                order,
                order_id,
                &exec_id,
                '2',
                code,
                *qty,
                component_side,
                0.0,
                &[],
                num_trades,
                &step::now_timestamp(),
            ),
            desc: format!(
                "成份券成交回报(类型2) ClOrdID={} 证券={} 过户数量={}",
                order.cl_ord_id, code, qty
            ),
            order_update: None,
            trade_seq: seq,
        });
    }

    // 类型3：资金替代成交（现金申赎固定有；实物申赎有现金替代时才有）
    if include_cash {
        seq += 1;
        let exec_id = stats.next_exec_id();
        plans.push(PlannedReport {
            delay_ms: st.trade_delay.sample(),
            kind: ReportKind::Trade,
            text: step::build_etf_trade(
                order,
                order_id,
                &exec_id,
                '3',
                "",
                0.0,
                etf_side,
                cash_total,
                &cash_groups,
                num_trades,
                &step::now_timestamp(),
            ),
            desc: format!(
                "资金替代成交回报(类型3) ClOrdID={} 现金替代总额={:.2} 分组={:?}",
                order.cl_ord_id, cash_total, cash_groups
            ),
            order_update: None,
            trade_seq: seq,
        });
    }

    (env.log)(
        "info",
        format!(
            "ETF 申赎回报计划：ClOrdID={} 类型1 + {} 只成份券 + {} 条资金替代（现金替代总额 {:.2}）",
            order.cl_ord_id,
            components.len(),
            if include_cash { 1 } else { 0 },
            cash_total
        ),
    );
    plans
}

/// 上交所 PCF 的市场 ID → 资金类型（1644 MarginAmtType）：
/// 101 沪市→1、102 深市→2、103 港市→3、其他→9。
fn market_amt_type(market_id: &str) -> i32 {
    match market_id.trim() {
        "101" => 1,
        "102" => 2,
        "103" => 3,
        _ => 9,
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Lua 脚本模式的回报计划。
///
/// 脚本里的 `ord` 表字段与 TCP 网关保持同名（ClOrdID/SecurityID/Side/
/// OrderQty/Price 等），另加 `ReqID`（数据库业务类型）方便脚本按业务分支。
/// 延迟只由脚本项的 `delayMs` 控制（与深/沪网关一致）。
fn plan_reports_lua(
    st: &StrategyConfig,
    order: &StepOrder,
    stats: &PlatformStats,
    env: &ScriptEnv,
) -> Vec<PlannedReport> {
    let items = match env.run(&st.lua_script, 0, |lua| {
        let t = lua.create_table()?;
        t.set("ReqID", order.reqid.as_str())?;
        t.set("MsgType", order.msg_type.as_str())?;
        t.set("ClOrdID", order.cl_ord_id.as_str())?;
        t.set("OrigClOrdID", order.orig_cl_ord_id.as_str())?;
        t.set("SecurityID", order.security_id.as_str())?;
        t.set("Side", order.side.as_str())?;
        t.set("OrdType", order.ord_type.as_str())?;
        t.set("OrderQty", order.qty)?;
        t.set("Price", order.price)?;
        t.set("AccountID", order.account.as_str())?;
        t.set("PBU", order.pbu.as_str())?;
        t.set("BranchID", order.branch.as_str())?;
        t.set("Text", order.text.as_str())?;
        Ok(t)
    }) {
        Ok(items) => items,
        Err(e) => {
            (env.log)(
                "error",
                format!(
                    "Lua 回报脚本执行失败，本笔委托 ClOrdID={} 不自动回报：{}",
                    order.cl_ord_id, e
                ),
            );
            return Vec::new();
        }
    };

    let mapping = biz_mapping(&order.reqid);
    let order_id = stats.next_order_id();
    let mut cum = 0.0f64;
    let mut plans = Vec::new();
    for item in items {
        match item.kind {
            ScriptKind::Confirm => {
                plans.push(PlannedReport {
                    delay_ms: item.delay_ms.unwrap_or(0),
                    kind: ReportKind::Ack,
                    text: step::build_ack(order, &order_id, &step::now_timestamp()),
                    desc: format!("确认回报 ClOrdID={}（Lua）", order.cl_ord_id),
                    order_update: Some(OrderUpdate {
                        order_id: order_id.clone(),
                        cum_qty: 0.0,
                        leaves_qty: order.qty,
                        status: OrderStatus::New,
                    }),
                    trade_seq: 0,
                });
            }
            ScriptKind::Trade => {
                if !mapping.has_exec_report {
                    (env.log)(
                        "warn",
                        format!(
                            "业务 {} 规范无成交回报，Lua 脚本的成交项已忽略（ClOrdID={}）",
                            order.reqid, order.cl_ord_id
                        ),
                    );
                    continue;
                }
                let remaining = (order.qty - cum).max(0.0);
                let qty = item.qty.unwrap_or(remaining).clamp(0.0, remaining);
                if qty <= 0.0 {
                    (env.log)(
                        "warn",
                        format!(
                            "Lua 成交项数量为 0 或已无剩余，已忽略（ClOrdID={}）",
                            order.cl_ord_id
                        ),
                    );
                    continue;
                }
                let px = item.price.unwrap_or(order.price);
                cum += qty;
                let leaves = (order.qty - cum).max(0.0);
                let filled = leaves <= 0.000001;
                let exec_id = stats.next_exec_id();
                plans.push(PlannedReport {
                    delay_ms: item.delay_ms.unwrap_or(0),
                    kind: ReportKind::Trade,
                    text: step::build_trade(
                        order,
                        &order_id,
                        &exec_id,
                        px,
                        qty,
                        leaves,
                        filled,
                        &step::now_timestamp(),
                    ),
                    desc: format!(
                        "成交回报 ClOrdID={} 价格={:.3} 数量={}（Lua）",
                        order.cl_ord_id, px, qty
                    ),
                    order_update: Some(OrderUpdate {
                        order_id: order_id.clone(),
                        cum_qty: cum,
                        leaves_qty: leaves,
                        status: if filled {
                            OrderStatus::Filled
                        } else {
                            OrderStatus::Partial
                        },
                    }),
                    trade_seq: plans.iter().filter(|p| p.kind == ReportKind::Trade).count() as i32 + 1,
                });
            }
            ScriptKind::Reject => {
                let reason = item
                    .reason
                    .map(|r| r as i64)
                    .unwrap_or(st.reject_reason as i64);
                let text = item.text.clone().unwrap_or_else(|| st.reject_text.clone());
                if item.via_business == Some(true) {
                    (env.log)(
                        "info",
                        "综合平台无独立业务拒绝消息，Lua 的 business 拒单已按执行报告拒绝发送"
                            .into(),
                    );
                }
                plans.push(PlannedReport {
                    delay_ms: item.delay_ms.unwrap_or(0),
                    kind: ReportKind::Reject,
                    text: step::build_reject(
                        order,
                        &order_id,
                        reason,
                        &text,
                        &step::now_timestamp(),
                    ),
                    desc: format!(
                        "拒单回报 ClOrdID={} 原因代码={}（Lua）",
                        order.cl_ord_id, reason
                    ),
                    order_update: Some(OrderUpdate {
                        order_id: order_id.clone(),
                        cum_qty: 0.0,
                        leaves_qty: 0.0,
                        status: OrderStatus::Rejected,
                    }),
                    trade_seq: 0,
                });
            }
        }
    }
    plans
}

/// 生成撤单响应（一条）。
///
/// 返回（计划, 是否撤单成功）。成功时同时给出原订单的缓存更新；
/// 拒单只回写撤单请求所在行，不改原订单状态。
pub fn plan_cancel(
    st: &StrategyConfig,
    cancel: &StepOrder,
    original: Option<&OrderEntry>,
) -> (PlannedReport, bool) {
    let success = match st.cancel_mode {
        CancelMode::AlwaysSuccess => true,
        CancelMode::AlwaysReject => false,
        CancelMode::Default => original.is_some_and(|o| o.status.is_inflight()),
    };
    if success {
        let withdraw = original.map(|o| o.leaves_qty).unwrap_or(cancel.qty);
        (
            PlannedReport {
                delay_ms: st.cancel_delay.sample(),
                kind: ReportKind::Cancel,
                text: step::build_cancel_success(cancel, withdraw, &step::now_timestamp()),
                desc: format!(
                    "撤单成功 ClOrdID={} OrigClOrdID={} 撤单数量={}",
                    cancel.cl_ord_id, cancel.orig_cl_ord_id, withdraw
                ),
                order_update: original.map(|o| OrderUpdate {
                    order_id: o.order_id.clone(),
                    cum_qty: o.cum_qty,
                    leaves_qty: 0.0,
                    status: OrderStatus::Cancelled,
                }),
                trade_seq: 0,
            },
            true,
        )
    } else {
        (
            PlannedReport {
                delay_ms: st.cancel_delay.sample(),
                kind: ReportKind::Reject,
                text: step::build_cancel_reject(
                    cancel,
                    st.cancel_reject_reason as i64,
                    &st.reject_text,
                    &step::now_timestamp(),
                ),
                desc: format!(
                    "撤单拒绝 ClOrdID={} OrigClOrdID={} 原因代码={}",
                    cancel.cl_ord_id, cancel.orig_cl_ord_id, st.cancel_reject_reason
                ),
                order_update: None,
                trade_seq: 0,
            },
            false,
        )
    }
}

/// 手动回复一笔在途订单（界面订单弹窗）：生成对应的 STEP 回报。
///
/// 与 TCP 网关的 build_manual_report 语义一致：
/// - 确认：保持订单状态不变（已报/部分成交）；
/// - 成交：数量缺省/超限按剩余量，价格缺省按委托价；无成交回报业务拒绝；
/// - 拒单：订单转已拒；
/// - 撤单：订单转已撤，撤单数量取剩余量。
pub fn build_manual_report(
    stats: &PlatformStats,
    entry: &OrderEntry,
    kind: crate::sz::session::ManualReportKind,
    qty: Option<f64>,
    price: Option<f64>,
    reason: Option<i32>,
) -> Result<PlannedReport, String> {
    use crate::sz::session::ManualReportKind;
    let order = order_from_entry(entry);
    let mapping = biz_mapping(&entry.biz);
    match kind {
        ManualReportKind::Ack => {
            let order_id = if entry.order_id.is_empty() {
                stats.next_order_id()
            } else {
                entry.order_id.clone()
            };
            Ok(PlannedReport {
                delay_ms: 0,
                kind: ReportKind::Ack,
                text: step::build_ack(&order, &order_id, &step::now_timestamp()),
                desc: format!(
                    "手动确认回报 ClOrdID={} OrderID={}",
                    entry.cl_ord_id,
                    order_id.trim_start_matches('0')
                ),
                order_update: Some(OrderUpdate {
                    order_id,
                    cum_qty: entry.cum_qty,
                    leaves_qty: entry.leaves_qty,
                    status: entry.status,
                }),
                trade_seq: 0,
            })
        }
        ManualReportKind::Trade => {
            if !mapping.has_exec_report {
                return Err(format!(
                    "业务[{}]无成交回报（综合平台业务表），不能手动回复成交",
                    entry.biz
                ));
            }
            // ETF 申赎的成交回报必须按 PCF 清单生成类型1/2/3 多条记录，
            // 手动回复只有一条普通成交，结构不符合 1.51 规范，直接拒绝。
            if etf_kind(&entry.biz).is_some() {
                return Err(format!(
                    "ETF 申赎业务[{}]的成交回报需按 PCF 清单自动生成，不支持手动成交；请使用自动回报模式",
                    entry.biz
                ));
            }
            if entry.leaves_qty < 1.0 {
                return Err(format!(
                    "订单 [{}] 剩余数量不足 1，不能手动回复成交",
                    entry.cl_ord_id
                ));
            }
            let fill_qty = qty
                .unwrap_or(entry.leaves_qty)
                .clamp(1.0, entry.leaves_qty);
            let fill_px = price.unwrap_or(entry.price).max(0.0001);
            let leaves = (entry.leaves_qty - fill_qty).max(0.0);
            let filled = leaves <= 0.000001;
            let order_id = if entry.order_id.is_empty() {
                stats.next_order_id()
            } else {
                entry.order_id.clone()
            };
            let exec_id = stats.next_exec_id();
            Ok(PlannedReport {
                delay_ms: 0,
                kind: ReportKind::Trade,
                text: step::build_trade(
                    &order,
                    &order_id,
                    &exec_id,
                    fill_px,
                    fill_qty,
                    leaves,
                    filled,
                    &step::now_timestamp(),
                ),
                desc: format!(
                    "手动成交回报 ClOrdID={} 价格={:.3} 数量={} 剩余={}",
                    entry.cl_ord_id, fill_px, fill_qty, leaves
                ),
                order_update: Some(OrderUpdate {
                    order_id,
                    cum_qty: entry.cum_qty + fill_qty,
                    leaves_qty: leaves,
                    status: if filled {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::Partial
                    },
                }),
                trade_seq: 1,
            })
        }
        ManualReportKind::Reject => {
            let order_id = if entry.order_id.is_empty() {
                stats.next_order_id()
            } else {
                entry.order_id.clone()
            };
            let reason = reason.unwrap_or(1) as i64;
            Ok(PlannedReport {
                delay_ms: 0,
                kind: ReportKind::Reject,
                text: step::build_reject(
                    &order,
                    &order_id,
                    reason,
                    "手动拒单",
                    &step::now_timestamp(),
                ),
                desc: format!(
                    "手动拒单回报 ClOrdID={} 原因代码={}",
                    entry.cl_ord_id, reason
                ),
                order_update: Some(OrderUpdate {
                    order_id,
                    cum_qty: 0.0,
                    leaves_qty: 0.0,
                    status: OrderStatus::Rejected,
                }),
                trade_seq: 0,
            })
        }
        ManualReportKind::Cancel => {
            let mut cancel = order.clone();
            cancel.orig_cl_ord_id = entry.cl_ord_id.clone();
            Ok(PlannedReport {
                delay_ms: 0,
                kind: ReportKind::Cancel,
                text: step::build_cancel_success(
                    &cancel,
                    entry.leaves_qty,
                    &step::now_timestamp(),
                ),
                desc: format!("手动撤单成功 ClOrdID={}", entry.cl_ord_id),
                order_update: Some(OrderUpdate {
                    order_id: entry.order_id.clone(),
                    cum_qty: entry.cum_qty,
                    leaves_qty: 0.0,
                    status: OrderStatus::Cancelled,
                }),
                trade_seq: 0,
            })
        }
    }
}

/// 从订单缓存记录还原一个 StepOrder（手动回复/回报构造用）
pub fn order_from_entry(entry: &OrderEntry) -> StepOrder {
    StepOrder {
        reqid: entry.biz.clone(),
        msg_type: step::MSG_NEW_ORDER.into(),
        cl_ord_id: entry.cl_ord_id.clone(),
        orig_cl_ord_id: String::new(),
        security_id: entry.security_id.clone(),
        side: match entry.side.as_str() {
            "买" => "1".into(),
            "卖" => "2".into(),
            other => other.to_string(),
        },
        ord_type: entry.ord_type.to_string(),
        price: entry.price,
        qty: entry.qty,
        parties: vec![
            Party::new(entry.account.clone(), step::party_role::ACCOUNT),
            Party::new(entry.pbu.clone(), step::party_role::PBU),
            Party::new(entry.branch.clone(), step::party_role::BRANCH),
        ]
        .into_iter()
        .filter(|p| !p.id.is_empty())
        .collect(),
        account: entry.account.clone(),
        pbu: entry.pbu.clone(),
        branch: entry.branch.clone(),
        owner_type: entry.owner_type as i64,
        text: entry.user_info.clone(),
        transact_time: String::new(),
    }
}

// ---------------------------------------------------------------------------
// 成交数量拆分（与 TCP 网关的规则保持一致，保证同一策略在各网关表现一致）
// ---------------------------------------------------------------------------

/// 一手 = 100（股票/基金份额；债券等按“手”申报的业务也以 100 为粒度拆分）
const LOT: f64 = 100.0;

fn gen_fills(st: &StrategyConfig, order: &StepOrder) -> Vec<(f64, f64)> {
    match st.mode {
        StrategyMode::FullSingle => vec![(order.qty, order.price)],
        StrategyMode::FullSplit => {
            let qtys = split_units(order.qty, sample_split_count(st));
            with_ladder_prices(st, order, &qtys)
        }
        StrategyMode::PartialSingle => {
            let part = partial_qty(order.qty);
            vec![(part, order.price)]
        }
        StrategyMode::PartialSplit => {
            let part = partial_qty(order.qty);
            let qtys = split_units(part, sample_split_count(st));
            with_ladder_prices(st, order, &qtys)
        }
        StrategyMode::NoAutoReply
        | StrategyMode::AckOnly
        | StrategyMode::Reject
        | StrategyMode::LuaScript => Vec::new(),
    }
}

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

/// 部分成交数量：按手随机，保证小于委托量（至少留 1 个单位）
fn partial_qty(total: f64) -> f64 {
    let units = (total / LOT).floor() as i64;
    if units >= 2 {
        LOT * rand::thread_rng().gen_range(1..=units - 1) as f64
    } else if total > 1.0 {
        (total / 2.0).floor().max(1.0)
    } else {
        total.max(1.0)
    }
}

/// 把总量随机拆成 n 份（尽量按整手；最后一份吃掉零头，确保总和相等）
fn split_units(total: f64, n: u32) -> Vec<f64> {
    let mut rng = rand::thread_rng();
    let units = (total / LOT).floor() as i64;
    let n = (n as i64).clamp(1, units.max(1)) as usize;
    if n <= 1 || units <= 1 {
        return vec![total];
    }
    let mut result = Vec::with_capacity(n);
    let mut remaining = units;
    for i in 0..n - 1 {
        let reserve = (n - 1 - i) as i64;
        let max_take = remaining - reserve;
        let take = rng.gen_range(1..=max_take);
        result.push(take as f64 * LOT);
        remaining -= take;
    }
    result.push(remaining as f64 * LOT + (total - units as f64 * LOT));
    result
}

/// 拆单价格阶梯：买单从委托价逐档向下、卖单逐档向上，最后一笔等于委托价
fn with_ladder_prices(
    st: &StrategyConfig,
    order: &StepOrder,
    qtys: &[f64],
) -> Vec<(f64, f64)> {
    let tick = if st.price_tick > 0.0 {
        st.price_tick
    } else {
        0.001
    };
    let n = qtys.len() as i64;
    qtys.iter()
        .enumerate()
        .map(|(i, &q)| {
            let steps = (n - 1 - i as i64) as f64;
            let px = if order.side == "2" {
                order.price + tick * steps
            } else {
                (order.price - tick * steps).max(tick)
            };
            (q, (px * 1000.0).round() / 1000.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::protocol::tag;
    use crate::config::{DelayConfig, RejectVia};
    use crate::lua_engine::ScriptCache;
    use crate::pcf::PcfComponent;
    use crate::positions::{OracleConnectConfig, PositionRow, PositionStore};

    fn test_order(qty: f64) -> StepOrder {
        StepOrder {
            reqid: "PFP".into(),
            msg_type: "D".into(),
            cl_ord_id: "ORDER00001".into(),
            security_id: "510300".into(),
            side: "1".into(),
            ord_type: "2".into(),
            price: 4.0,
            qty,
            parties: vec![
                Party::new("A123456789", 5),
                Party::new("PBU01", 1),
            ],
            account: "A123456789".into(),
            pbu: "PBU01".into(),
            ..Default::default()
        }
    }

    fn no_env() -> ScriptCache {
        ScriptCache::new()
    }

    #[test]
    fn full_single_plan_has_ack_and_one_trade() {
        let st = StrategyConfig::default();
        let order = test_order(1000.0);
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(&st, &order, &PlatformStats::default(), &env, None);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].kind, ReportKind::Ack);
        assert_eq!(plans[1].kind, ReportKind::Trade);
        let msg = step::StepMessage::parse(&plans[1].text).unwrap();
        assert_eq!(msg.get(tag::LAST_QTY), Some("1000"));
        assert_eq!(msg.get(tag::LEAVES_QTY), Some("0"));
    }

    #[test]
    fn ack_only_has_no_trade() {
        let st = StrategyConfig {
            mode: StrategyMode::AckOnly,
            ..Default::default()
        };
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(&st, &test_order(1000.0), &PlatformStats::default(), &env, None);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Ack);
    }

    #[test]
    fn business_without_exec_report_only_acks() {
        let mut order = test_order(1000.0);
        order.reqid = "CBT".into();
        let st = StrategyConfig::default();
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(&st, &order, &PlatformStats::default(), &env, None);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Ack);
    }

    #[test]
    fn reject_plan_uses_configured_reason() {
        let st = StrategyConfig {
            mode: StrategyMode::Reject,
            reject_via: RejectVia::BusinessReject,
            reject_reason: 1025,
            ..Default::default()
        };
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(&st, &test_order(1000.0), &PlatformStats::default(), &env, None);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, ReportKind::Reject);
        let msg = step::StepMessage::parse(&plans[0].text).unwrap();
        assert_eq!(msg.get(tag::ORD_REJ_REASON), Some("1025"));
        assert_eq!(msg.get(tag::EXEC_TYPE), Some("8"));
    }

    #[test]
    fn split_plan_keeps_total_quantity() {
        let st = StrategyConfig {
            mode: StrategyMode::FullSplit,
            split_count_min: 3,
            split_count_max: 3,
            price_tick: 0.01,
            ack_delay: DelayConfig::default(),
            ..Default::default()
        };
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(&st, &test_order(1000.0), &PlatformStats::default(), &env, None);
        let trades: Vec<_> = plans
            .iter()
            .filter(|p| p.kind == ReportKind::Trade)
            .collect();
        assert_eq!(trades.len(), 3);
        let total: f64 = trades
            .iter()
            .map(|p| {
                step::StepMessage::parse(&p.text)
                    .unwrap()
                    .get(tag::LAST_QTY)
                    .unwrap()
                    .parse::<f64>()
                    .unwrap()
            })
            .sum();
        assert_eq!(total, 1000.0);
        // 最后一笔剩余量必须为 0
        let last = step::StepMessage::parse(&trades[2].text).unwrap();
        assert_eq!(last.get(tag::LEAVES_QTY), Some("0"));
    }

    #[test]
    fn cancel_mode_default_follows_order_status() {
        let st = StrategyConfig::default();
        let mut cancel = test_order(0.0);
        cancel.msg_type = "F".into();
        cancel.cl_ord_id = "CANCEL001".into();
        cancel.orig_cl_ord_id = "ORDER00001".into();
        let (plan, ok) = plan_cancel(&st, &cancel, None);
        assert!(!ok);
        assert_eq!(plan.kind, ReportKind::Reject);
        let msg = step::StepMessage::parse(&plan.text).unwrap();
        assert_eq!(msg.msg_type, "9");

        let entry = OrderEntry {
            cl_ord_id: "ORDER00001".into(),
            order_id: "1".into(),
            security_id: "510300".into(),
            side: "买".into(),
            price: 4.0,
            qty: 1000.0,
            cum_qty: 0.0,
            leaves_qty: 1000.0,
            status: OrderStatus::New,
            ord_type: 2,
            account: "A".into(),
            branch: String::new(),
            ts: String::new(),
            conn_id: 1,
            pbu: "PBU01".into(),
            biz_id: 0,
            biz_pbu: String::new(),
            owner_type: 0,
            credit_tag: String::new(),
            clearing_firm: String::new(),
            user_info: String::new(),
            biz: "PFP".into(),
        };
        let (plan, ok) = plan_cancel(&st, &cancel, Some(&entry));
        assert!(ok);
        assert_eq!(plan.kind, ReportKind::Cancel);
        let msg = step::StepMessage::parse(&plan.text).unwrap();
        assert_eq!(msg.get(tag::EXEC_TYPE), Some("6"));
        assert_eq!(msg.get(tag::LAST_QTY), Some("1000"));
    }

    /// 上交所 PCF 测试数据：1 只允许现金替代（溢价/折价 10%）、
    /// 1 只必须现金替代、1 只禁止现金替代（实物申赎时过户）。
    fn test_sse_pcf() -> PcfFile {
        PcfFile {
            security_id: "510300".into(),
            creation_redemption_unit: 900000.0,
            mechanism: 1,
            components: vec![
                PcfComponent {
                    security_id: "000001".into(),
                    security_id_source: "102".into(),
                    component_share: 1600.0,
                    substitute_flag: 1,
                    creation_cash_substitute: 17408.0,
                    redemption_cash_substitute: 17408.0,
                    creation_premium_rate: 0.1,
                    redemption_discount_rate: 0.1,
                },
                PcfComponent {
                    security_id: "600000".into(),
                    security_id_source: "101".into(),
                    component_share: 0.0,
                    substitute_flag: 2,
                    creation_cash_substitute: 4950.0,
                    redemption_cash_substitute: 4950.0,
                    creation_premium_rate: 0.0,
                    redemption_discount_rate: 0.0,
                },
                PcfComponent {
                    security_id: "000002".into(),
                    security_id_source: "102".into(),
                    component_share: 1100.0,
                    substitute_flag: 0,
                    creation_cash_substitute: 0.0,
                    redemption_cash_substitute: 0.0,
                    creation_premium_rate: 0.0,
                    redemption_discount_rate: 0.0,
                },
            ],
        }
    }

    #[test]
    fn etf_cash_creation_uses_pcf_cash_substitute() {
        let st = StrategyConfig::default();
        let mut order = test_order(900000.0);
        order.reqid = "EEC".into();
        let pcf = test_sse_pcf();
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(
            &st,
            &order,
            &PlatformStats::default(),
            &env,
            Some(&pcf),
        );
        // 确认 + 类型1 + 类型3
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].kind, ReportKind::Ack);
        let t1 = step::StepMessage::parse(&plans[1].text).unwrap();
        assert_eq!(t1.get(tag::TRADE_REPORT_TYPE), Some("1"));
        assert_eq!(t1.get(tag::SECURITY_ALT_ID), Some("510300"));
        assert_eq!(t1.get(tag::LAST_QTY), Some("900000"));
        assert_eq!(t1.get(tag::SIDE), Some("1"));
        assert_eq!(t1.get(tag::NUM_TRADES), Some("2"));
        let t3 = step::StepMessage::parse(&plans[2].text).unwrap();
        assert_eq!(t3.get(tag::TRADE_REPORT_TYPE), Some("3"));
        assert_eq!(t3.get(tag::LAST_QTY), Some("0"));
        assert_eq!(t3.get(tag::SIDE), Some("1"));
        assert_eq!(t3.get(tag::NO_LEGS), Some("1"));
        assert_eq!(t3.get(tag::MARGIN_AMT_TYPE), Some("9"));
        // 17408×1.1=19148.8；+4950=24098.8；1 个篮子
        assert_eq!(t3.get(tag::TOTAL_VALUE_TRADED), Some("24098.800"));
    }

    #[test]
    fn etf_physical_creation_emits_components_and_grouped_cash() {
        let st = StrategyConfig::default();
        let mut order = test_order(900000.0);
        order.reqid = "IEC".into();
        let pcf = test_sse_pcf();
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(
            &st,
            &order,
            &PlatformStats::default(),
            &env,
            Some(&pcf),
        );
        // 确认 + 类型1 + 2 只成份券(000001/000002) + 类型3
        assert_eq!(plans.len(), 5);
        let trades: Vec<&PlannedReport> = plans
            .iter()
            .filter(|p| p.kind == ReportKind::Trade)
            .collect();
        assert_eq!(trades.len(), 4);
        let t1 = step::StepMessage::parse(&trades[0].text).unwrap();
        assert_eq!(t1.get(tag::TRADE_REPORT_TYPE), Some("1"));
        assert_eq!(t1.get(tag::SIDE), Some("1"));
        // 成份券过户方向与 ETF 相反（申购时投资者付出成份券=卖）
        let c1 = step::StepMessage::parse(&trades[1].text).unwrap();
        assert_eq!(c1.get(tag::TRADE_REPORT_TYPE), Some("2"));
        assert_eq!(c1.get(tag::SECURITY_ALT_ID), Some("000001"));
        assert_eq!(c1.get(tag::LAST_QTY), Some("1600"));
        assert_eq!(c1.get(tag::SIDE), Some("2"));
        let c2 = step::StepMessage::parse(&trades[2].text).unwrap();
        assert_eq!(c2.get(tag::SECURITY_ALT_ID), Some("000002"));
        assert_eq!(c2.get(tag::LAST_QTY), Some("1100"));
        // 资金替代按市场分组：101→1（600000 4950），102→2（000001 19148.8）
        let t3 = step::StepMessage::parse(&trades[3].text).unwrap();
        assert_eq!(t3.get(tag::TRADE_REPORT_TYPE), Some("3"));
        assert_eq!(t3.get(tag::TOTAL_VALUE_TRADED), Some("24098.800"));
        let groups: Vec<&str> = t3
            .fields
            .iter()
            .filter(|(t, _)| *t == tag::MARGIN_AMT_TYPE)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(groups, vec!["1", "2"]);
        assert_eq!(t3.get(tag::NUM_TRADES), Some("4"));
    }

    /// 实物申购（IEC）按持仓调整后的清单生成回报：
    /// - 允许现金替代的 000001 持仓 800 / 每篮子 1600 → 类型2 过户 800，
    ///   剩余 800 股转现金替代（17408 × 0.5 × 1.1 = 9574.4）；
    /// - 退补现金替代的 600003 不产生类型2，金额 2000 计入资金替代；
    /// - 禁止现金替代的 000002 持仓充足，类型2 过户 1100。
    #[test]
    fn etf_physical_creation_uses_position_adjusted_pcf() {
        let store = PositionStore::new();
        store.replace(
            OracleConnectConfig::default(),
            "60500052".into(),
            vec![
                PositionRow {
                    security_id: "000001".into(),
                    creation_redemption_qty: 800.0,
                    ..Default::default()
                },
                PositionRow {
                    security_id: "000002".into(),
                    creation_redemption_qty: 1100.0,
                    ..Default::default()
                },
            ],
        );
        let pcf = PcfFile {
            security_id: "510300".into(),
            creation_redemption_unit: 900000.0,
            mechanism: 1,
            components: vec![
                PcfComponent {
                    security_id: "000001".into(),
                    security_id_source: "102".into(),
                    component_share: 1600.0,
                    substitute_flag: 1,
                    creation_cash_substitute: 17408.0,
                    redemption_cash_substitute: 17408.0,
                    creation_premium_rate: 0.1,
                    redemption_discount_rate: 0.1,
                },
                PcfComponent {
                    security_id: "600003".into(),
                    security_id_source: "101".into(),
                    component_share: 500.0,
                    substitute_flag: 3,
                    creation_cash_substitute: 2000.0,
                    redemption_cash_substitute: 2000.0,
                    ..Default::default()
                },
                PcfComponent {
                    security_id: "600000".into(),
                    security_id_source: "101".into(),
                    component_share: 0.0,
                    substitute_flag: 2,
                    creation_cash_substitute: 4950.0,
                    redemption_cash_substitute: 4950.0,
                    ..Default::default()
                },
                PcfComponent {
                    security_id: "000002".into(),
                    security_id_source: "102".into(),
                    component_share: 1100.0,
                    substitute_flag: 0,
                    ..Default::default()
                },
            ],
        };
        let adjusted = store.adjust_creation_pcf(&pcf, 900000.0).unwrap();
        let st = StrategyConfig::default();
        let mut order = test_order(900000.0);
        order.reqid = "IEC".into();
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(
            &st,
            &order,
            &PlatformStats::default(),
            &env,
            Some(&adjusted),
        );
        // 确认 + 类型1 + 2 只成份券（000001/000002）+ 类型3
        assert_eq!(plans.len(), 5);
        let trades: Vec<&PlannedReport> = plans
            .iter()
            .filter(|p| p.kind == ReportKind::Trade)
            .collect();
        let c1 = step::StepMessage::parse(&trades[1].text).unwrap();
        assert_eq!(c1.get(tag::SECURITY_ALT_ID), Some("000001"));
        assert_eq!(c1.get(tag::LAST_QTY), Some("800"), "只过户持仓覆盖部分");
        let c2 = step::StepMessage::parse(&trades[2].text).unwrap();
        assert_eq!(c2.get(tag::SECURITY_ALT_ID), Some("000002"));
        assert_eq!(c2.get(tag::LAST_QTY), Some("1100"));
        let t3 = step::StepMessage::parse(&trades[3].text).unwrap();
        // 9574.4（000001 未覆盖部分）+ 2000（600003 退补）+ 4950（600000 必须）
        assert_eq!(t3.get(tag::TOTAL_VALUE_TRADED), Some("16524.400"));
        let groups: Vec<&str> = t3
            .fields
            .iter()
            .filter(|(t, _)| *t == tag::MARGIN_AMT_TYPE)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(groups, vec!["1", "2"]);
    }

    #[test]
    fn etf_without_pcf_still_emits_structured_zero_reports() {
        let st = StrategyConfig::default();
        let mut order = test_order(900000.0);
        order.reqid = "EEC".into();
        let cache = no_env();
        let env = ScriptEnv {
            cache: &cache,
            category: "shzh",
            gateway: "gw",
            platform: "p",
            peer: "db",
            log: &|_, _| {},
        };
        let plans = plan_reports(&st, &order, &PlatformStats::default(), &env, None);
        assert_eq!(plans.len(), 3, "无清单仍应回确认 + 类型1 + 类型3（金额 0）");
        let t3 = step::StepMessage::parse(&plans[2].text).unwrap();
        assert_eq!(t3.get(tag::TOTAL_VALUE_TRADED), Some("0.000"));
        assert_eq!(t3.get(tag::MARGIN_AMT_TYPE), Some("9"));
    }
}
