//! 上海综合网关的平台运行任务：轮询 SQL Server 委托表 → 模拟回报 → 写回接口表。
//!
//! # 运行模型
//!
//! 一个平台对应一个 tokio 异步任务（`run_platform`），不监听端口：
//! 1. 连接数据库并初始化 localrespnum/execnum 序号（从表内最大值继续）；
//! 2. 每 `poll_interval_ms` 毫秒查一次 `reqresp` 里 `ordstatus='R'` 的委托；
//! 3. 每笔委托 spawn 一个子任务，按策略延迟逐条写回（确认/成交/拒单）；
//! 4. 数据库断开时轮询任务持续重试，不影响其它 TCP 网关。
//!
//! # 为什么每笔委托单独 spawn
//!
//! 策略里的“回报延迟”可能很长（例如 5 秒），如果串行处理，一个慢委托会
//! 挡住后面所有委托。spawn 后各委托按自己的时间线执行；真正写库时由
//! [`DbHandle`] 的异步锁串行化，保证同一连接上不会并发 SQL。
//!
//! # 伪连接
//!
//! 界面上的“连接/报文”沿用 TCP 网关的模型：这里注册一个 conn_id=1 的
//! 伪连接（peer 显示数据库地址），把读到的 reqtext 记为“收”、写出的
//! resptext/execreporttext 记为“发”，报文弹窗和持久化照常可用。
//! 数据库断开时只把 loggedOn 置 false，平台本身仍在运行（继续重试）。

use crate::capture::ConnRecorder;
use crate::config::StrategyMode;
use crate::event::ConnInfo;
use crate::orderbook::{OrderEntry, OrderStatus, OrderUpdate};
use crate::pcf::PcfFile;
use crate::stats::PlatformStats;
use crate::sz::session::{ManualReportKind, SessionCtx};
use parking_lot::Mutex as PlMutex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::db::{DbHandle, DbReqRow};
use super::protocol::{self as step, StepMessage, StepOrder};
use super::strategy::{self as strategy, PlannedReport, ReportKind};

/// 数据库平台的伪连接序号（单连接模型，固定 1）
pub const DB_CONN_ID: u64 = 1;

/// 数据库地址显示文本：host:port/database
fn peer_label(db: &crate::config::DbConfig) -> String {
    format!(
        "{}:{}/{}",
        db.host.trim(),
        db.port,
        db.database.trim()
    )
}

/// 加载上海 ETF 清单（仅 EEC/EER/IEC/IER 申赎业务）。
///
/// 目录为空 = 未启用（静默返回 None，不告警）；目录已配置但找不到
/// `ssepcf_<代码>_<日期>.xml` 或解析失败时推送告警，并返回 None
/// （策略层按 0 只成份券/0 现金替代生成结构完整的回报）。
/// 走平台级 `pcf_cache`（与深交所共用缓存结构，但按市场标志区分），
/// 避免大目录逐笔全量扫描。
fn load_etf_pcf(ctx: &SessionCtx, dir: &str, order: &StepOrder) -> Option<Arc<PcfFile>> {
    let dir = dir.trim();
    if dir.is_empty() {
        return None;
    }
    if !matches!(order.reqid.as_str(), "EEC" | "EER" | "IEC" | "IER") {
        return None;
    }
    let code = order.security_id.trim();
    if code.is_empty() {
        ctx.log(
            "warn",
            format!(
                "ETF 申赎（{}）没有证券代码，无法查找上海 PCF 清单",
                order.reqid
            ),
        );
        return None;
    }
    match ctx.pcf_cache.lock().load_sse(Path::new(dir), code) {
        Ok(Some((path, pcf))) => {
            ctx.log(
                "info",
                format!(
                    "上海ETF清单：按 [{}] 解析 {} 只成份证券（{}）",
                    path.display(),
                    pcf.components.len(),
                    order.reqid
                ),
            );
            Some(pcf)
        }
        Ok(None) => {
            ctx.log(
                "warn",
                format!(
                    "上海ETF清单目录 [{}] 中未找到代码 [{}] 的清单文件（应为 ssepcf_{}_<日期>.xml），本笔按 0 只成份券/0 现金替代回报",
                    dir, code, code
                ),
            );
            None
        }
        Err(e) => {
            ctx.log(
                "warn",
                format!(
                    "上海ETF清单目录 [{}] 不可用（{}），本笔按 0 只成份券/0 现金替代回报",
                    dir, e
                ),
            );
            None
        }
    }
}

/// 平台运行入口（由 engine 在启动网关时 spawn）。
pub async fn run_platform(
    ctx: SessionCtx,
    db: Arc<DbHandle>,
    mut shutdown: watch::Receiver<bool>,
) {
    let peer = peer_label(db.config());
    register_conn(&ctx, &peer, false);
    ctx.stats.total_connections.fetch_add(1, Ordering::Relaxed);
    // 记录器挂在 ctx.recorders 里，后续 capture_send/capture_recv 按 conn_id 取
    let _ = ctx.start_capture_peer(DB_CONN_ID, &peer);
    ctx.log(
        "info",
        format!("正在连接 SQL Server {}（委托表 {} / 成交表 {}）",
            peer,
            db.config().req_resp_table,
            db.config().exec_report_table
        ),
    );

    // ---- 1) 连接数据库并初始化序号（失败每 2 秒重试，直到停止） ----
    let mut last_err = String::new();
    loop {
        if *shutdown.borrow() {
            cleanup(&ctx, &db).await;
            return;
        }
        match db.init_sequences().await {
            Ok(()) => {
                update_conn_state(&ctx, true);
                ctx.log(
                    "info",
                    format!(
                        "SQL Server 已连接：{}；委托表={} 成交表={}",
                        peer,
                        db.config().req_resp_table,
                        db.config().exec_report_table
                    ),
                );
                break;
            }
            Err(e) => {
                update_conn_state(&ctx, false);
                if e != last_err {
                    ctx.log("error", format!("连接数据库失败：{}", e));
                    last_err = e;
                }
                if sleep_or_stop(2000, &mut shutdown).await {
                    cleanup(&ctx, &db).await;
                    return;
                }
            }
        }
    }

    // ---- 2) 轮询委托表 ----
    let interval_ms = db.config().poll_interval_ms.clamp(10, 60_000);
    let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
    // 上一轮处理超过间隔时不要“补跑”多次，直接跳到下一个 tick
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // 正在处理的 reqnum 集合：带延迟的委托仍是 ordstatus='R'，
    // 防止下一次轮询把同一笔重复 spawn
    let pending: Arc<PlMutex<HashSet<i32>>> = Arc::new(PlMutex::new(HashSet::new()));
    let mut last_poll_err = String::new();

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        if *shutdown.borrow() {
            break;
        }
        match db.load_pending(200).await {
            Ok(rows) => {
                if !last_poll_err.is_empty() {
                    ctx.log("info", "数据库连接已恢复".into());
                    update_conn_state(&ctx, true);
                    last_poll_err.clear();
                }
                for row in rows {
                    if !pending.lock().insert(row.reqnum) {
                        continue;
                    }
                    let ctx2 = ctx.clone();
                    let db2 = db.clone();
                    let pending2 = pending.clone();
                    let shutdown2 = shutdown.clone();
                    tokio::spawn(async move {
                        process_row(ctx2, db2, row, pending2, shutdown2).await;
                    });
                }
            }
            Err(e) => {
                if e != last_poll_err {
                    ctx.log("warn", format!("读取委托表失败，{}", e));
                    last_poll_err = e;
                }
                update_conn_state(&ctx, false);
            }
        }
    }

    cleanup(&ctx, &db).await;
    ctx.log("info", "上海综合平台已停止".into());
}

/// 单笔委托/撤单的处理外壳：统一记录错误并清理 pending 标记。
async fn process_row(
    ctx: SessionCtx,
    db: Arc<DbHandle>,
    row: DbReqRow,
    pending: Arc<PlMutex<HashSet<i32>>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let reqnum = row.reqnum;
    // 先把原始 reqtext 记进报文记录器（解析失败也保留原文）
    let msg = StepMessage::parse(&row.reqtext).ok();
    capture_recv(&ctx, &row.reqtext, msg.as_ref());
    let result = process_row_inner(&ctx, &db, &row, msg, &mut shutdown).await;
    if let Err(e) = result {
        ctx.log(
            "error",
            format!(
                "处理委托失败 reqnum={} reqid={} reff={}：{}",
                row.reqnum, row.reqid, row.reff, e
            ),
        );
    }
    pending.lock().remove(&reqnum);
}

async fn process_row_inner(
    ctx: &SessionCtx,
    db: &Arc<DbHandle>,
    row: &DbReqRow,
    msg: Option<StepMessage>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let Some(msg) = msg else {
        // 报文解析失败：标记为 'E' 防止轮询反复读到同一行；错误已由外层记录
        db.mark_status(row.reqnum, "E").await?;
        return Err("STEP 报文解析失败".into());
    };
    match msg.msg_type.as_str() {
        step::MSG_NEW_ORDER => process_new_order(ctx, db, row, &msg, shutdown).await,
        step::MSG_CANCEL => process_cancel(ctx, db, row, &msg, shutdown).await,
        other => {
            db.mark_status(row.reqnum, "E").await?;
            Err(format!("不支持的 STEP 消息类型: {}", other))
        }
    }
}

/// 新委托：登记订单缓存 → 按策略生成回报 → 逐条写库。
async fn process_new_order(
    ctx: &SessionCtx,
    db: &Arc<DbHandle>,
    row: &DbReqRow,
    msg: &StepMessage,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let order = match StepOrder::parse(msg, &row.reqid) {
        Ok(o) => o,
        Err(e) => {
            // 解析失败的行标记 'E'，避免轮询反复读到同一行刷日志
            db.mark_status(row.reqnum, "E").await?;
            return Err(e);
        }
    };

    // 同一 ClOrdID 若已对应另一条 reqnum，说明柜台重复写了委托行：
    // 与真实 EzSTEP 后台的防重语义一致，直接标记已处理，不重复回报。
    if let Some(prev) = db.order_reqnum(&order.cl_ord_id) {
        if prev != row.reqnum {
            ctx.log(
                "warn",
                format!(
                    "重复委托：ClOrdID={} 已由 reqnum={} 处理，忽略 reqnum={}",
                    order.cl_ord_id, prev, row.reqnum
                ),
            );
            db.mark_status(row.reqnum, "O").await?;
            return Ok(());
        }
    }
    // 先记住映射：手动回复/后续重复判断都要用；ack 写失败重试时 reqnum 不变
    let first_seen = db.order_reqnum(&order.cl_ord_id).is_none();
    db.remember_order(&order.cl_ord_id, row.reqnum);
    if first_seen {
        ctx.stats.orders.fetch_add(1, Ordering::Relaxed);
    }

    // 登记订单缓存（关闭缓存时跳过；关闭后撤单默认按“找不到原单”处理）。
    // 重试路径上订单已在缓存里，不能重复 push。
    if let Some(ob) = &ctx.orders {
        if ob.find(&order.cl_ord_id).is_none() {
            let entry = order_entry_from_step(&order, row);
            ob.add(entry);
        }
    }

    // 策略配置在块作用域内克隆，读锁不跨 await 存活
    let st = ctx.strategy.read().unwrap().clone();
    let peer = peer_label(db.config());
    // ETF 现金/实物申赎：按“ETF清单文件路径”找 ssepcf_<代码>_<日期>.xml
    // （目录未配置时静默跳过；配置了但没找到/解析失败会告警）
    let pcf = load_etf_pcf(ctx, &st.etf_pcf_dir, &order);
    // 实物申购（IEC）按“同步持仓数据”调整成份券：允许现金替代的优先
    // 扣持仓、不足部分转现金替代；禁止现金替代的持仓不足则整笔拒单。
    // 现金申购（EEC）不扣持仓；赎回、关闭自动回报、Lua 模式、自动拒单、
    // 只回确认也不参与（这些模式不会生成按 PCF 计算的成交明细）
    let is_physical_creation = order.reqid.as_str() == "IEC";
    let apply_positions = is_physical_creation
        && order.qty > 0.0
        && !matches!(
            st.mode,
            StrategyMode::NoAutoReply
                | StrategyMode::LuaScript
                | StrategyMode::Reject
                | StrategyMode::AckOnly
        );
    let mut adjusted_pcf: Option<PcfFile> = None;
    let mut position_reject: Option<String> = None;
    if apply_positions {
        if let Some(p) = pcf.as_deref() {
            match ctx.positions.adjust_creation_pcf(p, order.qty) {
                Ok(adj) => adjusted_pcf = Some(adj),
                Err(reason) => {
                    ctx.log(
                        "warn",
                        format!(
                            "上海 ETF 清单 [{}] 持仓校验未通过，本笔申购拒单：{}",
                            p.security_id, reason
                        ),
                    );
                    position_reject = Some(reason);
                }
            }
        }
    }
    let plans = {
        let env = crate::lua_engine::ScriptEnv {
            cache: ctx.lua_cache.as_ref(),
            category: "shzh",
            gateway: &ctx.gateway_name,
            platform: &ctx.cfg.name,
            peer: &peer,
            log: &|level, msg| ctx.log(level, msg),
        };
        match position_reject.as_deref() {
            Some(reason) => strategy::plan_force_reject(&st, &order, &ctx.stats, reason),
            None => {
                // 申购用按持仓调整过的清单；赎回/未启用时用原始清单
                let pcf_ref = adjusted_pcf.as_ref().or(pcf.as_deref());
                strategy::plan_reports(&st, &order, &ctx.stats, &env, pcf_ref)
            }
        }
    };

    if plans.is_empty() {
        // “关闭自动回报”：只登记缓存，等界面手动回复。
        // ordstatus 改成 'O' 让轮询不再读到；localrespnum 保持 0，
        // 柜台按 localrespnum 增量读取，因此不会看到这条（符合“不自动回”）。
        db.mark_status(row.reqnum, "O").await?;
        ctx.log(
            "info",
            format!(
                "已登记委托 ClOrdID={} 业务={}（关闭自动回报，等待手动回复）",
                order.cl_ord_id, row.reqid
            ),
        );
        return Ok(());
    }

    let mut wrote_reqresp = false;
    for plan in plans {
        if sleep_or_stop(plan.delay_ms, shutdown).await || *shutdown.borrow() {
            return Ok(());
        }
        if execute_plan(
            ctx,
            db,
            &order,
            row.reqnum,
            &order.cl_ord_id,
            &plan,
            false,
        )
        .await?
        {
            wrote_reqresp = true;
        }
    }
    // Lua 脚本只写成交、没有确认时，委托行仍是 'R'；这里补一个“已处理”
    // 标记，避免下一次轮询把同一笔委托的成交再插一遍。
    if !wrote_reqresp {
        db.mark_status(row.reqnum, "O").await?;
    }
    Ok(())
}

/// 撤单请求：按 CancelMode 决定成功/拒绝，回写到撤单请求所在行。
async fn process_cancel(
    ctx: &SessionCtx,
    db: &Arc<DbHandle>,
    row: &DbReqRow,
    msg: &StepMessage,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let cancel = match StepOrder::parse(msg, &row.reqid) {
        Ok(o) => o,
        Err(e) => {
            db.mark_status(row.reqnum, "E").await?;
            return Err(e);
        }
    };
    ctx.stats.cancels.fetch_add(1, Ordering::Relaxed);
    let st = ctx.strategy.read().unwrap().clone();
    let original = ctx
        .orders
        .as_ref()
        .and_then(|ob| ob.find(&cancel.orig_cl_ord_id));
    let (plan, _success) = strategy::plan_cancel(&st, &cancel, original.as_ref());
    if sleep_or_stop(plan.delay_ms, shutdown).await || *shutdown.borrow() {
        return Ok(());
    }
    // 撤单成功时订单缓存更新的是“原订单”，不是撤单请求自身
    let update_target = cancel.orig_cl_ord_id.clone();
    execute_plan(
        ctx,
        db,
        &cancel,
        row.reqnum,
        &update_target,
        &plan,
        true,
    )
    .await?;
    Ok(())
}

/// 执行一条回报：写 reqresp 或 execreport、更新订单缓存、统计、记报文。
///
/// `reqnum` 是本次要回写的委托行；`update_target` 是订单缓存里要更新的
/// ClOrdID（新委托=自身，撤单成功=原订单）。返回是否写了 reqresp
/// （成交只写成交表，返回 false）。
async fn execute_plan(
    ctx: &SessionCtx,
    db: &Arc<DbHandle>,
    order: &StepOrder,
    reqnum: i32,
    update_target: &str,
    plan: &PlannedReport,
    is_cancel: bool,
) -> Result<bool, String> {
    match plan.kind {
        ReportKind::Trade => {
            let mapping = step::biz_mapping(&order.reqid);
            if !mapping.has_exec_report {
                // 理论上策略层已拦截；这里再兜底，避免给不存在的业务写成交行
                ctx.log(
                    "warn",
                    format!(
                        "业务 {} 无成交回报，已丢弃成交项（ClOrdID={}）",
                        order.reqid, order.cl_ord_id
                    ),
                );
                return Ok(false);
            }
            db.insert_exec_report(
                mapping.bcast_type,
                mapping.set_id,
                plan.trade_seq,
                &plan.text,
            )
            .await?;
            ctx.stats.trades.fetch_add(1, Ordering::Relaxed);
            capture_send(ctx, &plan.text);
            apply_order_update(ctx, update_target, &plan.order_update);
            ctx.log("info", plan.desc.clone());
            Ok(false)
        }
        ReportKind::Ack => {
            db.write_reqresp_response(reqnum, "O", &plan.text).await?;
            ctx.stats.acks.fetch_add(1, Ordering::Relaxed);
            capture_send(ctx, &plan.text);
            apply_order_update(ctx, update_target, &plan.order_update);
            ctx.log("info", plan.desc.clone());
            Ok(true)
        }
        ReportKind::Reject => {
            // 撤单请求本身已处理（结果为拒绝），委托行标 'O'；
            // 新委托被拒则标 'E'（结果由 resptext 里的 ExecType=8 表达）
            let status = if is_cancel { "O" } else { "E" };
            db.write_reqresp_response(reqnum, status, &plan.text).await?;
            // 撤单拒绝不算“委托拒单”；只有新委托拒绝才计入 order_rejects
            if !is_cancel {
                ctx.stats.order_rejects.fetch_add(1, Ordering::Relaxed);
            }
            capture_send(ctx, &plan.text);
            apply_order_update(ctx, update_target, &plan.order_update);
            ctx.log("info", plan.desc.clone());
            Ok(true)
        }
        ReportKind::Cancel => {
            db.write_reqresp_response(reqnum, "O", &plan.text).await?;
            capture_send(ctx, &plan.text);
            apply_order_update(ctx, update_target, &plan.order_update);
            ctx.log("info", plan.desc.clone());
            Ok(true)
        }
    }
}

/// 由 STEP 委托构造订单缓存记录
fn order_entry_from_step(order: &StepOrder, row: &DbReqRow) -> OrderEntry {
    let side = match order.side.as_str() {
        "1" => "买",
        "2" => "卖",
        other => other,
    };
    OrderEntry {
        cl_ord_id: order.cl_ord_id.clone(),
        order_id: String::new(),
        security_id: order.security_id.clone(),
        side: side.to_string(),
        price: order.price,
        qty: order.qty,
        cum_qty: 0.0,
        leaves_qty: order.qty,
        status: OrderStatus::New,
        ord_type: order.ord_type.parse().unwrap_or(0),
        account: order.account.clone(),
        branch: order.branch.clone(),
        ts: chrono::Local::now().format("%H:%M:%S").to_string(),
        conn_id: DB_CONN_ID,
        pbu: if order.pbu.is_empty() {
            row.pbu.clone()
        } else {
            order.pbu.clone()
        },
        biz_id: 0,
        biz_pbu: order.pbu.clone(),
        owner_type: order.owner_type.clamp(0, u16::MAX as i64) as u16,
        credit_tag: String::new(),
        clearing_firm: String::new(),
        user_info: order.text.clone(),
        biz: row.reqid.clone(),
    }
}

/// 写库成功后同步订单缓存（终态保护由 OrderBook::apply 内部保证）
fn apply_order_update(ctx: &SessionCtx, cl_ord_id: &str, update: &Option<OrderUpdate>) {
    if let (Some(ob), Some(upd)) = (&ctx.orders, update) {
        ob.apply(cl_ord_id, upd);
    }
}

/// 记录发出的 STEP 报文（解析失败只记原始文本）
fn capture_send(ctx: &SessionCtx, text: &str) {
    let rec = ctx.recorders.lock().unwrap().get(&DB_CONN_ID).cloned();
    if let Some(rec) = rec {
        let fields = StepMessage::parse(text)
            .map(|m| m.describe())
            .unwrap_or_default();
        rec.record_send(text.as_bytes(), fields);
    }
}

/// 记录读到的 STEP 报文
fn capture_recv(ctx: &SessionCtx, text: &str, msg: Option<&StepMessage>) {
    let rec = ctx.recorders.lock().unwrap().get(&DB_CONN_ID).cloned();
    if let Some(rec) = rec {
        let fields = msg.map(|m| m.describe()).unwrap_or_default();
        rec.record_recv(text.as_bytes(), fields);
    }
}

/// 更新/插入伪连接状态（数据库连接成功 = 已登录）
fn register_conn(ctx: &SessionCtx, peer: &str, logged_on: bool) {
    let mut conns = ctx.connections.lock().unwrap();
    conns.insert(
        DB_CONN_ID,
        ConnInfo {
            id: DB_CONN_ID,
            peer: peer.to_string(),
            comp_id: "DB".into(),
            logged_on,
            since: chrono::Local::now().format("%H:%M:%S").to_string(),
        },
    );
}

fn update_conn_state(ctx: &SessionCtx, connected: bool) {
    let mut conns = ctx.connections.lock().unwrap();
    if let Some(c) = conns.get_mut(&DB_CONN_ID) {
        c.logged_on = connected;
    }
}

/// 关闭数据库连接并清理伪连接（平台停止时）
async fn cleanup(ctx: &SessionCtx, db: &DbHandle) {
    db.close().await;
    ctx.connections.lock().unwrap().remove(&DB_CONN_ID);
    if let Some(rec) = ctx.recorders.lock().unwrap().get(&DB_CONN_ID).cloned() {
        rec.mark_dead();
    }
}

/// 等待 ms 毫秒；期间收到停止信号返回 true。
/// 回报延迟可能很长（例如 10 秒），停止网关时应立即取消，而不是等延迟走完。
async fn sleep_or_stop(ms: u64, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    if ms == 0 {
        return false;
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(ms)) => false,
        _ = shutdown.changed() => true,
    }
}

/// 界面手动回复（成交/确认/拒单/撤单）：生成 STEP 回报并写库。
///
/// 与自动回报共用同一套报文构造与写库逻辑；返回（日志描述, 订单缓存更新）。
/// `recorder` 为伪连接记录器（关闭报文展示时为 None）。
pub async fn manual_report(
    db: &DbHandle,
    stats: &PlatformStats,
    entry: &OrderEntry,
    kind: ManualReportKind,
    qty: Option<f64>,
    price: Option<f64>,
    reason: Option<i32>,
    recorder: Option<&Arc<ConnRecorder>>,
) -> Result<(String, OrderUpdate), String> {
    let plan = strategy::build_manual_report(stats, entry, kind, qty, price, reason)?;
    let reqnum = db
        .find_reqnum(&entry.cl_ord_id)
        .await?
        .ok_or_else(|| format!("委托表里找不到订单 {}（reff）", entry.cl_ord_id))?;
    match plan.kind {
        ReportKind::Trade => {
            let mapping = step::biz_mapping(&entry.biz);
            db.insert_exec_report(
                mapping.bcast_type,
                mapping.set_id,
                plan.trade_seq,
                &plan.text,
            )
            .await?;
        }
        ReportKind::Ack | ReportKind::Cancel => {
            db.write_reqresp_response(reqnum, "O", &plan.text).await?;
        }
        ReportKind::Reject => {
            db.write_reqresp_response(reqnum, "E", &plan.text).await?;
        }
    }
    if let Some(rec) = recorder {
        let fields = StepMessage::parse(&plan.text)
            .map(|m| m.describe())
            .unwrap_or_default();
        rec.record_send(plan.text.as_bytes(), fields);
    }
    let update = plan
        .order_update
        .ok_or_else(|| "内部错误：手动回报缺少订单状态更新".to_string())?;
    Ok((plan.desc, update))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DbConfig;

    #[test]
    fn peer_label_formats_db_address() {
        let cfg = DbConfig {
            host: "10.1.1.5".into(),
            port: 1433,
            database: "ezstep".into(),
            ..Default::default()
        };
        assert_eq!(peer_label(&cfg), "10.1.1.5:1433/ezstep");
    }

    #[test]
    fn order_entry_maps_step_fields() {
        let order = StepOrder {
            reqid: "PFP".into(),
            msg_type: "D".into(),
            cl_ord_id: "ORDER001".into(),
            security_id: "510300".into(),
            side: "1".into(),
            ord_type: "2".into(),
            price: 4.5,
            qty: 1000.0,
            account: "A1".into(),
            pbu: "PBU01".into(),
            branch: "888".into(),
            owner_type: 1,
            ..Default::default()
        };
        let row = DbReqRow {
            reqnum: 7,
            reqid: "PFP".into(),
            reff: "ORDER001".into(),
            pbu: "PBU01".into(),
            security_id: "510300".into(),
            record_timestamp: String::new(),
            reqtext: String::new(),
        };
        let e = order_entry_from_step(&order, &row);
        assert_eq!(e.cl_ord_id, "ORDER001");
        assert_eq!(e.side, "买");
        assert_eq!(e.leaves_qty, 1000.0);
        assert_eq!(e.biz, "PFP");
        assert_eq!(e.conn_id, DB_CONN_ID);
        assert_eq!(e.pbu, "PBU01");
    }
}
