//! 上海竞价 TCP 会话管理：Logon/Logout/心跳/委托处理
//!
//! # 连接生命周期
//!
//! 复用 sz::session 的会话框架，仅协议流程与报文号不同：
//!
//! ```text
//! 柜台(OMS)连上 TCP
//!   → 等待 Logon 登录报文（可选校验密码）
//!   → 登录成功：回复 Logon 确认 + 下发平台信息(209) + 平台状态(208)
//!   → OMS 发起执行回报同步 ExecRptSync(206)
//!     以 ExecRptSyncRsp(207) 回报各分区序号与已确认序号
//!     同步完成后才允许发送委托
//!   → 进入正常工作期：
//!       - 双方按约定间隔互发心跳
//!       - 收到委托(58) → 按策略回送确认(32)/成交(103)/拒绝(204)
//!       - 收到撤单(61) → 按订单真实状态回撤单成功(32)或撤单失败(59)
//!       - 收到注册处理申报(301) → 回注册处理执行回报(302)（SetID=992）
//!       - 收到网络密码服务申报(306) → 回申报响应(308)（不进执行报告流）
//!   → 结束：柜台注销 / 连接断开 / 心跳超时 / 网关停止
//! ```
//!
//! # MsgSeqNum 序号同步
//!
//! 上交所按分区各自维护发送序号（从 1 起递增），
//! 每条发出的报文都必须带上本分区的发送序号。
//! writer 任务发送前由 protocol::finalize_seq 统一补号，
//! 因此报文捕获的字节与线上实际发送完全一致。

use crate::config::PlatformConfig;
use crate::event::ConnInfo;
use crate::orderbook::{OrderEntry, OrderStatus, OrderUpdate};
use crate::stats::PlatformStats;
use crate::sz::session::{ManualReportKind, SessionCtx};
use super::protocol::{
    self as protocol, designation_instruction, exec_type, msg_type, ord_status, platform_state,
    BIZ_ID_DESIGNATION, BIZ_ID_DESIGNATION_CANCEL, BIZ_ID_PWD_SERVICE, SET_ID_DESIGNATION,
    CancelOrder, CancelReject, ExecRpt, ExtendFields, Logon, Logout, NewOrder, OrderReject,
    PasswordServiceOrder, PasswordServiceRsp, RegistrationOrder, RegistrationRpt, SyncRspGroup,
    TradeRpt,
};
use super::strategy::{self as strategy, PlannedReport, ReportKind};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

/// 全局连接序号发生器：每来一个新连接就 +1（与 sz 同构）。
static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// 读取一条完整报文：16 字节报文头 + 消息体 + 4 字节校验和。
///
/// 报文头 16 字节：MsgType(4) + MsgSeqNum(8) + MsgBodyLen(4)，
/// 校验和单独 4 字节跟在消息体后；idle_timeout 同时作用在“报文头/消息体/
/// 校验和”三步读取上——对端发完头停住不发剩余部分时也会超时断开，
/// 否则会话会被半包连接永久挂起（占死单连接槽位）。
/// 返回 (消息类型, 消息体, 校验是否通过, 完整原始帧)。
async fn read_frame(
    rh: &mut OwnedReadHalf,
    idle_timeout: Duration,
) -> std::io::Result<(u32, Vec<u8>, bool, Vec<u8>)> {
    // 带超时读满 buf：超时/读错统一转成 io::Error，由调用方结束会话
    async fn read_exact_timeout(
        rh: &mut OwnedReadHalf,
        buf: &mut [u8],
        idle_timeout: Duration,
    ) -> std::io::Result<()> {
        match tokio::time::timeout(idle_timeout, rh.read_exact(buf)).await {
            // 内层是 read_exact 的 io 错误，直接透传（tokio 新版返回已读字节数，
            // 这里只关心成败）；外层是 timeout 的 Elapsed，转成 io::Error
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "读超时：空闲时间超过心跳间隔 3 倍",
            )),
        }
    }

    let mut head = [0u8; 16];
    read_exact_timeout(rh, &mut head, idle_timeout).await?;
    let mt = u32::from_be_bytes(head[0..4].try_into().unwrap());
    // head[4..12] 为 MsgSeqNum，发送时由 finalize_seq 统一填充
    let body_len = u32::from_be_bytes(head[12..16].try_into().unwrap()) as usize;
    // 消息体长度上限 8MB，防御异常大帧拖垮内存
    if body_len > 8 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("消息体长度非法: {}", body_len),
        ));
    }
    let mut body = vec![0u8; body_len];
    read_exact_timeout(rh, &mut body, idle_timeout).await?;
    let mut cks_buf = [0u8; 4];
    read_exact_timeout(rh, &mut cks_buf, idle_timeout).await?;
    let recv_cks = u32::from_be_bytes(cks_buf);
    let mut all = Vec::with_capacity(16 + body_len + 4);
    all.extend_from_slice(&head);
    all.extend_from_slice(&body);
    let ok = protocol::checksum(&all) == recv_cks;
    all.extend_from_slice(&cks_buf);
    Ok((mt, body, ok, all))
}

/// 会话入口：一个 TCP 连接一个任务，由 engine 在收到连接时 spawn。
///
/// 与 sz::session::handle_conn 同构；engine 按网关分类分派到这里。
pub async fn handle_conn(
    ctx: SessionCtx,
    stream: TcpStream,
    peer: SocketAddr,
    mut shutdown: watch::Receiver<bool>,
) {
    let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = stream.set_nodelay(true);
    let (mut rh, mut wh) = stream.into_split();

    let recorder = ctx.start_capture(conn_id, &peer);
    let rec_send = recorder.clone();

    // 回报帧先入 mpsc 队列，由独立 writer 任务串行写出
    // （上交所要求按分区序号递增发送，故必须串行）
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4096);
    // 登记发送通道：界面手动回复成交/拒单/撤单时，经此把回报投给本连接
    ctx.conn_tx.lock().unwrap().insert(conn_id, tx.clone());
    let stats = ctx.stats.clone();
    let writer = tokio::spawn(async move {
        let mut seq: u64 = 0;
        while let Some(mut buf) = rx.recv().await {
            seq += 1;
            protocol::finalize_seq(&mut buf, seq);
            // 回报记录号 ReportIndex 在真实发送时分配并补写进报文：
            // 分配时机与线上发送顺序严格一致，多笔并发回报（含延迟回报）也不会乱序；
            // 非回报消息（心跳/登录/204/207/308 等）跳过，不占回报序号
            if buf.len() >= 36 {
                let mt = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                if protocol::is_report_frame(mt) {
                    protocol::patch_report_index(&mut buf, stats.next_report_index() as u64);
                }
            }
            if let Some(r) = &rec_send {
                // 发送前捕获并解析字段：头 16 字节报文头，消息体在 [16..len-4]
                let fields = if buf.len() >= 20 {
                    let mt = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                    protocol::describe_fields(mt, &buf[16..buf.len() - 4])
                } else {
                    Vec::new()
                };
                r.record_send(&buf, fields);
            }
            if wh.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = wh.shutdown().await;
    });

    ctx.stats.total_connections.fetch_add(1, Ordering::Relaxed);
    ctx.connections.lock().unwrap().insert(
        conn_id,
        ConnInfo {
            id: conn_id,
            peer: peer.to_string(),
            comp_id: String::new(),
            logged_on: false,
            since: chrono::Local::now().format("%H:%M:%S").to_string(),
        },
    );
    ctx.log("info", format!("收到 {} 的连接，等待 Logon", peer));

    let mut hb_secs: u64 = 30; // 心跳间隔默认 30s；Logon 可覆盖，clamp 到 [5,60]
    let mut logged_on = false;
    // 登录后 OMS 会先发 ExecRptSync(206) 同步执行回报，
    // 收到对应 Rsp(207) 后才允许发委托；此前委托进 pending 缓冲
    let mut synced = false;
    let mut pending: Vec<PlannedReport> = Vec::new();
    let mut pbu = String::new(); // 分区编号：由 Logon 的 SenderCompID 前 8 位解析
    let mut hb_task: Option<tokio::task::JoinHandle<()>> = None;
    // 延迟回报任务登记表：会话结束必须全部 abort 并等其释放发送端，
    // 否则 writer.await 会一直等队列关闭（连接槽位被占死）。用 parking_lot
    // 锁：取锁即得 guard，无中毒路径
    let report_tasks: Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));

    loop {
        // 读超时 = 心跳间隔 × 3，至少 15 秒
        let idle = Duration::from_secs((hb_secs * 3).max(15));
        let frame = tokio::select! {
            _ = shutdown.changed() => {
                let _ = tx.send(Logout { session_status: 0, text: "网关停止".into() }.encode()).await;
                break;
            }
            r = read_frame(&mut rh, idle) => match r {
                Ok(f) => f,
                Err(e) => {
                    ctx.log("warn", format!("连接 {} 读失败: {}", peer, e));
                    break;
                }
            }
        };
        let (mt, body, cks_ok, raw) = frame;
        if let Some(r) = &recorder {
            // 捕获收到的报文（校验和错误的也记录），同时按字段解析
            r.record_recv(&raw, protocol::describe_fields(mt, &body));
        }
        if !cks_ok {
            // 校验和错误：回 Logout(5001) 后关闭
            ctx.log("warn", format!("连接 {} 校验和错误 MsgType={}，回注销后关闭", peer, mt));
            let _ = tx
                .send(Logout { session_status: 5001, text: "CheckSum Error".into() }.encode())
                .await;
            break;
        }

        match mt {
            msg_type::LOGON => {
                let logon = match Logon::decode(&body) {
                    Ok(l) => l,
                    Err(e) => {
                        ctx.log("error", format!("Logon 报文解析失败: {}", e));
                        break;
                    }
                };
                // 心跳间隔取 OMS 请求值，clamp 到 [5,60]；对端填 0 表示未指定
                // （协议约定），用默认 30 秒，避免 0 被钳成 5 秒过快心跳
                hb_secs = if logon.heart_bt_int == 0 {
                    30
                } else {
                    (logon.heart_bt_int as u64).clamp(5, 60)
                };
                // 分区编号 = SenderCompID 前 8 位（单分区场景即 PBU）
                pbu = logon.sender_comp_id.chars().take(8).collect();
                // 对端 Logon 的 SenderCompID 将成为我们的 TargetCompID
                let reply = Logon {
                    sender_comp_id: ctx.cfg.comp_id.clone(),
                    target_comp_id: logon.sender_comp_id.clone(),
                    heart_bt_int: hb_secs as u16,
                    prtcl_version: if logon.prtcl_version.is_empty() {
                        "0.50".into()
                    } else {
                        logon.prtcl_version.clone()
                    },
                    trade_date: protocol::now_date(),
                    qsize: logon.qsize,
                };
                let _ = tx.send(reply.encode()).await;
                // 登录应答后补发平台信息(209) + 平台状态(208)
                let _ = tx
                    .send(protocol::encode_platform_state(
                        ctx.cfg.platform_type,
                        platform_state::OPEN,
                    ))
                    .await;
                // 执行报告信息（208）：本 PBU 下的全部分区号（支持多分区配置），
                // OMS 按它初始化各分区的回报同步
                let parts: Vec<u32> = ctx.cfg.partitions().iter().map(|&p| p as u32).collect();
                let _ = tx
                    .send(protocol::encode_exec_rpt_info(
                        ctx.cfg.platform_type,
                        &[(pbu.as_str(), parts.as_slice())],
                    ))
                    .await;
                logged_on = true;
                if let Some(c) = ctx.connections.lock().unwrap().get_mut(&conn_id) {
                    c.comp_id = logon.sender_comp_id.clone();
                    c.logged_on = true;
                }
                ctx.log(
                    "info",
                    format!(
                        "连接 {} 登录成功 CompID={} 心跳={}s，等待执行回报同步",
                        peer, logon.sender_comp_id, hb_secs
                    ),
                );
                // 登录成功：启动心跳任务，立即回复 Logon 确认
                if let Some(h) = hb_task.take() {
                    h.abort();
                }
                let hb_tx = tx.clone();
                hb_task = Some(tokio::spawn(async move {
                    let mut iv = tokio::time::interval(Duration::from_secs(hb_secs));
                    iv.tick().await; // 跳过 interval 首次立即触发
                    loop {
                        iv.tick().await;
                        if hb_tx.send(protocol::encode_heartbeat()).await.is_err() {
                            break;
                        }
                    }
                }));
            }
            msg_type::HEARTBEAT => {
                // 收到心跳即刷新 read_frame 读超时，无需单独应答
            }
            msg_type::LOGOUT => {
                let status = Logout::decode(&body).map(|l| l.session_status).unwrap_or(0);
                ctx.log("info", format!("连接 {} 收到注销确认，会话状态={}", peer, status));
                let _ = tx
                    .send(Logout { session_status: 0, text: "Normal Logout".into() }.encode())
                    .await;
                break;
            }
            msg_type::EXEC_RPT_SYNC if logged_on => {
                handle_sync(&ctx, &tx, &body, &mut synced, &mut pending, &report_tasks).await;
            }
            msg_type::NEW_ORDER if logged_on => {
                handle_new_order(&ctx, &tx, &body, &pbu, synced, &mut pending, conn_id, &report_tasks).await;
            }
            msg_type::CANCEL_ORDER if logged_on => {
                handle_cancel(&ctx, &tx, &body, &pbu, synced, &mut pending).await;
            }
            msg_type::REGISTRATION if logged_on => {
                handle_registration(&ctx, &tx, &body, &pbu).await;
            }
            msg_type::PWD_SERVICE if logged_on => {
                handle_password_service(&ctx, &tx, &body).await;
            }
            other => {
                if !logged_on {
                    ctx.log("warn", format!("连接 {} 登录前收到业务报文 MsgType={}，忽略", peer, other));
                } else {
                    ctx.log("warn", format!("连接 {} 收到未知会话报文 MsgType={}，忽略", peer, other));
                }
            }
        }
    }

    // 会话结束：停心跳任务、等 writer 排空队列
    if let Some(h) = hb_task {
        h.abort();
    }
    // 先终止延迟回报任务：它们各自持有 tx 的 clone，不结束的话 writer 的
    // recv 永远等不到队列关闭，下面的 writer.await 会一直挂在这里，
    // connections 移除 / mark_dead 都执行不到（重连会被单连接限制拒绝）
    let pending_tasks = std::mem::take(&mut *report_tasks.lock());
    for h in &pending_tasks {
        h.abort();
    }
    for h in pending_tasks {
        let _ = h.await;
    }
    // 先移除发送通道：它持有一份 tx clone，不先移除的话 writer 的 recv
    // 永远等不到队列关闭，writer.await 会一直挂在这里，后续的
    // connections 移除 / mark_dead 都执行不到（重连会被单连接限制拒绝）
    ctx.conn_tx.lock().unwrap().remove(&conn_id);
    drop(tx);
    let _ = writer.await;
    ctx.connections.lock().unwrap().remove(&conn_id);
    // 记录器保留不删：平台级报文弹窗要能按连接分组回看本连接的历史报文
    if let Some(r) = &recorder {
        r.mark_dead();
    }
    ctx.log("info", format!("连接 {} 已关闭", peer));
}

/// 处理执行回报同步(206)：按各分区回 ExecRptSyncRsp(207)，置 synced=true。
///
/// 同步回应中 EndReportIndex 取当前已发送的回报序号，RejReason=0。
/// 注意：ReportIndex 现在由 writer 在真实发送时分配（与线上顺序一致），
/// 因此这里读到的全局计数器值就是“已实际发出”的回报条数，不再包含
/// 已分配未发送的序号（旧实现分配于生成时，会把尚未送出的序号也算进去）。
/// 同步完成前收到的委托在 pending 缓冲，完成后统一补发。
async fn handle_sync(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
    synced: &mut bool,
    pending: &mut Vec<PlannedReport>,
    report_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    let groups = match protocol::decode_exec_rpt_sync(body) {
        Ok(g) => g,
        Err(e) => {
            ctx.log("error", format!("执行回报同步(206)处理失败: {}", e));
            return;
        }
    };
    let end_index = ctx.stats.report_index.load(Ordering::Relaxed) as u64;
    let rsp: Vec<SyncRspGroup> = groups
        .iter()
        .map(|g| SyncRspGroup {
            pbu: g.pbu.clone(),
            set_id: g.set_id,
            begin_report_index: g.begin_report_index,
            end_report_index: end_index,
            rej_reason: 0,
            text: "OK".into(),
        })
        .collect();
    let _ = tx.send(protocol::encode_exec_rpt_sync_rsp(&rsp)).await;
    ctx.log(
        "info",
        format!("执行回报同步(206) 共 {} 个分区，发送同步回应(207)", groups.len()),
    );
    *synced = true;
    // 同步完成：补发缓冲期内暂存的委托
    if !pending.is_empty() {
        let buffered = std::mem::take(pending);
        ctx.log("info", format!("同步完成，补发缓冲的委托 {} 笔", buffered.len()));
        dispatch_reports(ctx, tx, buffered, report_tasks);
    }
}

/// 同步前缓冲的回报计划条数上限：OMS 一直不发 206 时防止内存无界增长
/// （正常 OMS 登录后立即同步，缓冲只会有少量委托）
const MAX_PENDING_PLANS: usize = 4096;

/// 处理委托(58)：按策略生成回报计划并发送。
/// 执行回报同步未完成时，委托先入 pending 缓冲等待补发。
async fn handle_new_order(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
    pbu: &str,
    synced: bool,
    pending: &mut Vec<PlannedReport>,
    conn_id: u64,
    report_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    let order = match NewOrder::decode(body) {
        Ok(o) => o,
        Err(e) => {
            ctx.log("error", format!("委托(58)处理失败: {}", e));
            return;
        }
    };
    // 表 3.2.1 之外的未知业务：前置校验不通过，回申报拒绝(204) 错误码 4012
    // （SecurityID 错误或者业务类型 BizID 错误），不登记订单缓存
    if !strategy::is_known_biz(order.biz_id) {
        ctx.stats.order_rejects.fetch_add(1, Ordering::Relaxed);
        let rej = OrderReject {
            biz_id: order.biz_id,
            biz_pbu: order.biz_pbu.clone(),
            cl_ord_id: order.cl_ord_id.clone(),
            security_id: order.security_id.clone(),
            ord_rej_reason: 4012,
            trade_date: protocol::now_date(),
            transact_time: protocol::now_ntime(),
            user_info: order.user_info.clone(),
        };
        let _ = tx.send(rej.encode()).await;
        ctx.log("warn", format!("未知业务 BizID={}，回申报拒绝(204) 4012", order.biz_id));
        return;
    }
    ctx.stats.orders.fetch_add(1, Ordering::Relaxed);
    // 协议整数还原：价格 ÷100000（5 位小数）、数量 ÷1000
    ctx.log(
        "info",
        format!(
            "收到委托 ClOrdID={} 证券={} 方向={} 价格={:.5} 数量={}",
            order.cl_ord_id,
            order.security_id,
            side_name(order.side),
            order.price as f64 / 100000.0,
            order.order_qty / 1000
        ),
    );

    // 缓存订单：登记一条新订单（状态=已报），回报逐条发出时再更新状态；
    // conn_id 记录订单来自哪个连接（手动回复成交/拒单/撤单时按它定位发送通道）
    if let Some(ob) = &ctx.orders {
        ob.add(OrderEntry {
            cl_ord_id: order.cl_ord_id.clone(),
            order_id: String::new(),
            security_id: order.security_id.clone(),
            side: side_name(order.side).to_string(),
            price: order.price as f64 / 100000.0,
            qty: order.order_qty as f64 / 1000.0,
            cum_qty: 0.0,
            leaves_qty: order.order_qty as f64 / 1000.0,
            status: OrderStatus::New,
            ord_type: order.ord_type,
            account: order.account.clone(),
            branch: order.branch_id.clone(),
            ts: chrono::Local::now().format("%H:%M:%S").to_string(),
            conn_id,
            // 协议回填字段：手动回复回报时与自动回报保持一致
            // （Pbu 空会导致柜台按异常路径处理回报，如显示为“已撤”）
            pbu: pbu.to_string(),
            biz_id: order.biz_id,
            biz_pbu: order.biz_pbu.clone(),
            owner_type: order.owner_type as u16,
            credit_tag: order.credit_tag.clone(),
            clearing_firm: order.clearing_firm.clone(),
            user_info: order.user_info.clone(),
            // 业务标识字符串：撤单/手动回复时按它反查业务特征（参照深市 ApplID 用法）
            biz: order.biz_id.to_string(),
        });
    }

    let plans = {
        let st = ctx.strategy.read().unwrap();
        strategy::plan_reports(&st, ctx.cfg.partition_for(&order.security_id), &order, &ctx.stats, pbu)
    };
    if !synced {
        // 同步未完成：委托暂存缓冲，待同步后补发
        if pending.len() + plans.len() > MAX_PENDING_PLANS {
            // 缓冲超上限：丢弃本笔回报并告警（OMS 一直不发 206 属于协议违规，
            // 不能让缓冲无限增长拖垮内存）
            ctx.log(
                "error",
                format!(
                    "同步前回报缓冲已满（{} 条），ClOrdID={} 的委托回报被丢弃",
                    MAX_PENDING_PLANS, order.cl_ord_id
                ),
            );
            return;
        }
        ctx.log(
            "warn",
            format!(
                "OMS 尚未完成执行回报同步(206)，ClOrdID={} 的委托已缓冲（共 {} 笔）",
                order.cl_ord_id,
                plans.len()
            ),
        );
        pending.extend(plans);
        return;
    }
    dispatch_reports(ctx, tx, plans, report_tasks);
}

/// 按计划发送回报：等待 delay_ms 后经 mpsc 队列交给 writer。
/// 发送走独立 spawn 任务而非直接 await，避免阻塞主读循环；
/// 队列有界（4096），发送端不会无限堆积；句柄登记到会话级任务表，
/// 会话结束时统一 abort（防止任务持有发送端导致清理挂起）。
fn dispatch_reports(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    plans: Vec<PlannedReport>,
    report_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) {
    let ctx2 = ctx.clone();
    let tx2 = tx.clone();
    let h = tokio::spawn(async move {
        for p in plans {
            if p.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(p.delay_ms)).await;
            }
            // 发送前检查订单缓存：订单已到终态（如延迟期间被撤单成功/已成交/已拒）
            // 时，丢弃尚未发出的回报——撤单成功后不再补发此前规划的确认/成交，
            // 保证线上回报序列与订单状态一致（撤单成功回报本身不受此限）
            if p.kind != ReportKind::Cancel {
                if let Some(ob) = &ctx2.orders {
                    if ob
                        .find(&p.cl_ord_id)
                        .is_some_and(|o| o.status.is_terminal())
                    {
                        continue;
                    }
                }
            }
            if tx2.send(p.frame).await.is_err() {
                break; // 连接已关闭，停止发送
            }
            match p.kind {
                ReportKind::Ack => ctx2.stats.acks.fetch_add(1, Ordering::Relaxed),
                ReportKind::Trade => ctx2.stats.trades.fetch_add(1, Ordering::Relaxed),
                ReportKind::Reject => ctx2.stats.order_rejects.fetch_add(1, Ordering::Relaxed),
                ReportKind::BusinessReject => {
                    ctx2.stats.business_rejects.fetch_add(1, Ordering::Relaxed)
                }
                // 撤单成功回报不占统计计数（那是撤单请求/成交/拒单的计数）
                ReportKind::Cancel => 0,
            };
            // 回报真实发出后，把该笔回报对应的订单状态同步进缓存
            // （计划里带着委托编号：缓冲补发时多笔委托的计划混在一起也能对号入座）
            if let Some(upd) = &p.order_update {
                if let Some(ob) = &ctx2.orders {
                    ob.apply(&p.cl_ord_id, upd);
                }
            }
            ctx2.log("info", format!("发送: {}", p.desc));
        }
    });
    report_tasks.lock().push(h);
}

/// 处理撤单(61)：平台开启“缓存订单”时按订单真实状态回复：
/// - 原单在途（已报/部分成交）→ 撤单成功：回执行报告(32) ExecType=4 已撤
/// - 原单已是终态（全成/已拒/已撤）或找不到 → 撤单失败：回撤单失败(59)
/// 未开启缓存时维持旧行为（一律撤单失败）。
///
/// 执行回报同步未完成时，撤单成功/失败回报同样进 pending 缓冲等 207 后补发：
/// 否则柜台会先收到撤单成功、后收到该委托延迟补发的申报确认/成交，状态错乱
/// （补发时发送侧会按订单缓存终态过滤掉已撤单委托的确认/成交）。
async fn handle_cancel(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
    pbu: &str,
    synced: bool,
    pending: &mut Vec<PlannedReport>,
) {
    let req = match CancelOrder::decode(body) {
        Ok(r) => r,
        Err(e) => {
            ctx.log("error", format!("撤单(61)处理失败: {}", e));
            return;
        }
    };
    ctx.stats.cancels.fetch_add(1, Ordering::Relaxed);
    // 表 3.2.1 业务特征：决定回报分区号与是否支持撤单
    let biz = strategy::biz_info(req.biz_id);
    let set_id = if biz.set_id == 0 { ctx.cfg.partition_for(&req.security_id) as u32 } else { biz.set_id };
    // 表 3.2.1 撤单列不支持撤单的业务：直接回撤单失败
    if !biz.allow_cancel {
        send_cancel_reject(ctx, tx, &req, pbu, set_id, synced, pending).await;
        return;
    }

    // ---- 有订单缓存：先尝试按原单状态撤单 ----
    if let Some(ob) = &ctx.orders {
        // 原单存在且在途才允许撤（find 拿的是撤单前的快照，剩余量用于回报）
        if let Some(entry) = ob.find(&req.orig_cl_ord_id) {
            if entry.status.is_inflight() {
                ob.cancel_inflight(&req.orig_cl_ord_id); // 缓存状态置为已撤
                // 撤单成功回报：32 执行报告，ExecType=4 / OrdStatus=4（已撤）
                let cxl = ExecRpt {
                    pbu: pbu.to_string(),
                    set_id,
                    report_index: 0, // 发送时由 writer 任务补写
                    biz_id: req.biz_id,
                    exec_type: exec_type::CANCELLED,
                    biz_pbu: req.biz_pbu.clone(),
                    cl_ord_id: req.cl_ord_id.clone(),
                    security_id: req.security_id.clone(),
                    account: entry.account.clone(),
                    owner_type: req.owner_type,
                    side: req.side,
                    price: (entry.price * 100000.0) as i64,
                    order_qty: (entry.qty * 1000.0) as i64,
                    leaves_qty: 0,
                    cxl_qty: (entry.leaves_qty * 1000.0) as i64,
                    ord_type: entry.ord_type,
                    time_in_force: 0,
                    ord_status: ord_status::CANCELLED,
                    credit_tag: String::new(),
                    orig_cl_ord_id: req.orig_cl_ord_id.clone(),
                    clearing_firm: String::new(),
                    branch_id: entry.branch.clone(),
                    ord_rej_reason: 0,
                    // 本撤单回报分配新确认编号，原订单编号回填到 OrigOrdCnfmID
                    ord_cnfm_id: ctx.stats.next_order_id(),
                    orig_ord_cnfm_id: entry.order_id.clone(),
                    trade_date: protocol::now_date(),
                    transact_time: protocol::now_ntime(),
                    user_info: req.user_info.clone(),
                    // 4.3.3.1 说明 2：撤单成功响应的扩展字段与原单对应业务一致
                    extend: ExtendFields::default(),
                };
                if !synced {
                    // 同步未完成：撤单成功回报进缓冲，与委托回报一起等 207 后按
                    // 到达顺序补发（排在原委托回报之后，原委托的确认/成交会被
                    // 发送侧的终态检查丢弃，柜台只收到撤单成功）
                    pending.push(PlannedReport {
                        delay_ms: 0,
                        kind: ReportKind::Cancel,
                        frame: cxl.encode(),
                        desc: format!(
                            "撤单成功(32) ClOrdID={} OrigClOrdID={}（同步后补发）",
                            req.cl_ord_id, req.orig_cl_ord_id
                        ),
                        cl_ord_id: req.orig_cl_ord_id.clone(),
                        order_update: Some(OrderUpdate {
                            order_id: entry.order_id.clone(),
                            cum_qty: entry.cum_qty,
                            leaves_qty: 0.0,
                            status: OrderStatus::Cancelled,
                        }),
                    });
                } else {
                    let _ = tx.send(cxl.encode()).await;
                }
                ctx.log(
                    "info",
                    format!(
                        "收到撤单 ClOrdID={} OrigClOrdID={}，原单在途，已回撤单成功(32){}",
                        req.cl_ord_id,
                        req.orig_cl_ord_id,
                        if synced { "" } else { "（同步后补发）" }
                    ),
                );
                return;
            }
        }
    }

    // ---- 撤单失败：不支持撤单 / 未开启缓存 / 找不到原单 / 原单已是终态 ----
    send_cancel_reject(ctx, tx, &req, pbu, set_id, synced, pending).await;
}

/// 构造并发送撤单失败(59)：业务不支持撤单 / 未开启缓存 / 找不到原单 / 原单已是终态。
/// 同步未完成时进 pending 缓冲（与撤单成功同一规则，等 207 后补发）。
async fn send_cancel_reject(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    req: &CancelOrder,
    pbu: &str,
    set_id: u32,
    synced: bool,
    pending: &mut Vec<PlannedReport>,
) {
    let rej = CancelReject {
        pbu: pbu.to_string(),
        set_id,
        report_index: 0, // 发送时由 writer 任务补写
        biz_id: req.biz_id,
        biz_pbu: req.biz_pbu.clone(),
        cl_ord_id: req.cl_ord_id.clone(),
        security_id: req.security_id.clone(),
        orig_cl_ord_id: req.orig_cl_ord_id.clone(),
        branch_id: req.branch_id.clone(),
        cxl_rej_reason: 1, // 1 = 未知订单（或原单不可撤）
        trade_date: protocol::now_date(),
        transact_time: protocol::now_ntime(),
        user_info: req.user_info.clone(),
    };
    let desc = format!(
        "收到撤单 ClOrdID={} OrigClOrdID={}，原单不存在或不可撤，回撤单失败(59)",
        req.cl_ord_id, req.orig_cl_ord_id
    );
    if !synced {
        // 同步未完成：撤单失败也是执行报告流消息（带 ReportIndex），
        // 同样进缓冲等 207 后补发，避免柜台在同步前收到执行报告
        pending.push(PlannedReport {
            delay_ms: 0,
            kind: ReportKind::Cancel,
            frame: rej.encode(),
            desc: format!("{}（同步后补发）", desc),
            cl_ord_id: req.orig_cl_ord_id.clone(),
            order_update: None, // 撤单失败不改订单状态
        });
    } else {
        let _ = tx.send(rej.encode()).await;
    }
    ctx.log("info", desc);
}

/// 处理注册处理申报(301)：回注册处理执行回报(302)。
///
/// 302 带 Pbu/SetID(=992)/ReportIndex，编入执行报告流（占回报序号）；
/// 仅支持两种组合（4.4.1 说明 1）：
/// - 指定登记：BizID=300200、SecurityID=799999、注册指令='1'、注册类型='1'
/// - 指定撤销：BizID=300201、SecurityID=799998、注册指令='2'、注册类型='1'
/// 组合校验不通过回 ExecType=8/OrdStatus=8 的拒绝响应（错误码 4012）。
async fn handle_registration(
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
    pbu: &str,
) {
    let req = match RegistrationOrder::decode(body) {
        Ok(r) => r,
        Err(e) => {
            ctx.log("error", format!("注册处理申报(301)处理失败: {}", e));
            return;
        }
    };
    // 注册处理申报不是委托（新订单），不计入“委托数”统计口径
    let ok = match req.biz_id {
        BIZ_ID_DESIGNATION => {
            req.security_id == "799999"
                && req.designation_instruction == designation_instruction::REGISTER
                && req.designation_trans_type == b'1'
        }
        BIZ_ID_DESIGNATION_CANCEL => {
            req.security_id == "799998"
                && req.designation_instruction == designation_instruction::CANCEL
                && req.designation_trans_type == b'1'
        }
        _ => false,
    };
    let rpt = RegistrationRpt {
        pbu: pbu.to_string(),
        set_id: SET_ID_DESIGNATION,
        report_index: 0, // 发送时由 writer 任务补写
        biz_id: req.biz_id,
        exec_type: if ok { exec_type::NEW } else { exec_type::REJECT },
        biz_pbu: req.biz_pbu.clone(),
        cl_ord_id: req.cl_ord_id.clone(),
        security_id: req.security_id.clone(),
        account: req.account.clone(),
        owner_type: req.owner_type,
        ord_status: if ok { ord_status::NEW } else { ord_status::REJECTED },
        orig_cl_ord_id: String::new(),
        branch_id: String::new(),
        ord_rej_reason: if ok { 0 } else { 4012 }, // 组合不合法：SecurityID 或 BizID 错误
        ord_cnfm_id: if ok { ctx.stats.next_order_id() } else { String::new() },
        orig_ord_cnfm_id: String::new(),
        trade_date: protocol::now_date(),
        transact_time: protocol::now_ntime(),
        user_info: req.user_info.clone(),
    };
    let _ = tx.send(rpt.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到注册处理申报(301) BizID={} SecurityID={} 指令={} 类型={}，回执行回报(302) {}",
            req.biz_id,
            req.security_id,
            req.designation_instruction as char,
            req.designation_trans_type as char,
            if ok { "申报成功" } else { "申报拒绝" }
        ),
    );
}

/// 处理网络密码服务申报(306)：回申报响应(308)。
///
/// 响应无 Pbu/SetID/ReportIndex，不进执行报告流（表 3.2.1 注 2），
/// 也不占回报序号；OrdRejReason 成功时返回 0。该业务不进行重单校验。
async fn handle_password_service(ctx: &SessionCtx, tx: &mpsc::Sender<Vec<u8>>, body: &[u8]) {
    let req = match PasswordServiceOrder::decode(body) {
        Ok(r) => r,
        Err(e) => {
            ctx.log("error", format!("网络密码服务申报(306)处理失败: {}", e));
            return;
        }
    };
    // 网络密码服务申报不是委托（新订单），不计入“委托数”统计口径
    let ok = req.biz_id == BIZ_ID_PWD_SERVICE;
    let rsp = PasswordServiceRsp {
        biz_id: req.biz_id,
        biz_pbu: req.biz_pbu.clone(),
        cl_ord_id: req.cl_ord_id.clone(),
        security_id: req.security_id.clone(),
        account: req.account.clone(),
        owner_type: req.owner_type,
        branch_id: req.branch_id.clone(),
        side: req.side,
        validation_code: req.validation_code.clone(),
        ord_rej_reason: if ok { 0 } else { 4012 },
        trade_date: protocol::now_date(),
        transact_time: protocol::now_ntime(),
        user_info: req.user_info.clone(),
    };
    let _ = tx.send(rsp.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到网络密码服务申报(306) ClOrdID={} SecurityID={} Side={}，回申报响应(308) {}",
            req.cl_ord_id,
            req.security_id,
            req.side as char,
            if ok { "成功" } else { "拒绝" }
        ),
    );
}

/// 构造一笔手动回复报文（成交/拒单/撤单成功），与 sz::session 同构，
/// 仅报文格式与放大倍数不同：价格放大 10 万倍、数量放大 1000 倍。
///
/// 返回（编码好的完整帧, 日志描述, 订单缓存同步更新）；
/// 表 3.2.1 无成交确认的业务不允许手动回复成交（返回 Err）。
/// qty/price 用自然单位（股/元）；成交数量默认剩余量（全成）、价格默认
/// 委托价、拒单原因默认 1；订单缓存没登记过的协议特有字段用空串占位。
pub fn build_manual_report(
    cfg: &PlatformConfig,
    stats: &PlatformStats,
    entry: &OrderEntry,
    kind: ManualReportKind,
    qty: Option<f64>,
    price: Option<f64>,
    reason: Option<i32>,
    front_reject: bool,
) -> Result<(Vec<u8>, String, OrderUpdate), String> {
    match kind {
        ManualReportKind::Ack => {
            // 确认回报：执行报告(32) ExecType='0'（申报成功），订单保持“已报”
            // 状态、可继续回复成交/拒单/撤单。订单确认编号尚无则新分配
            let mut rpt = base_manual_rpt(cfg, stats, entry);
            if rpt.ord_cnfm_id.is_empty() {
                rpt.ord_cnfm_id = stats.next_order_id();
            }
            let desc = format!(
                "手动确认回报(32) ClOrdID={} OrdCnfmID={}",
                entry.cl_ord_id,
                rpt.ord_cnfm_id.trim_start_matches('0')
            );
            let update = OrderUpdate {
                order_id: rpt.ord_cnfm_id.clone(),
                cum_qty: entry.cum_qty,
                leaves_qty: entry.leaves_qty,
                status: OrderStatus::New,
            };
            Ok((rpt.encode(), desc, update))
        }
        ManualReportKind::Trade => {
            // 表 3.2.1 无成交确认的业务（如转托管/划转）不能手动回复成交：
            // 这类业务只有申报响应，真实柜台不会收到成交回报
            let biz = strategy::biz_info(entry.biz_id);
            if !biz.allow_trade {
                return Err(format!(
                    "业务[{}]无成交回报（表 3.2.1），不能手动回复成交",
                    biz.name
                ));
            }
            // 剩余量不足 1 股（小数股委托或已基本成交）时拒绝手动成交：
            // 否则下方 clamp(1, 0) 会触发 panic（min > max）
            if entry.leaves_qty < 1.0 {
                return Err(format!(
                    "订单 [{}] 剩余数量不足 1 股，不能手动回复成交",
                    entry.cl_ord_id
                ));
            }
            // 成交数量钳制到 (0, 剩余量]：不传或超限都按剩余量全成
            let fill_shares = qty
                .map(|q| q as i64)
                .unwrap_or(entry.leaves_qty as i64)
                .clamp(1, entry.leaves_qty as i64);
            let fill_px = price.unwrap_or(entry.price).max(0.0001);
            let leaves = (entry.leaves_qty - fill_shares as f64).max(0.0);
            let filled = leaves == 0.0;
            let last_px = (fill_px * 100000.0).round() as i64;
            let last_qty = fill_shares * 1000;
            // 现货竞价用登录分区号，其余业务用表 3.2.1 固定 SetID
            let set_id = if biz.set_id == 0 { cfg.partition_for(&entry.security_id) as u32 } else { biz.set_id };
            let trade = TradeRpt {
                pbu: entry.pbu.clone(),
                set_id,
                report_index: 0, // 发送时由 writer 任务补写
                biz_id: entry.biz_id,
                exec_type: exec_type::TRADE,
                biz_pbu: entry.biz_pbu.clone(),
                cl_ord_id: entry.cl_ord_id.clone(),
                security_id: entry.security_id.clone(),
                account: entry.account.clone(),
                owner_type: entry.owner_type as u8,
                order_entry_time: protocol::now_ntime(),
                last_px,
                last_qty,
                // 成交金额 = 价格 × 数量，两者都是放大整数，除回数量放大倍数；
                // 用 i128 中间量防极端价格×数量相乘溢出 i64（先除后乘会丢精度）
                gross_trade_amt: ((last_px as i128) * (last_qty as i128) / 1000) as i64,
                side: side_byte(&entry.side),
                order_qty: (entry.qty * 1000.0).round() as i64,
                leaves_qty: (leaves * 1000.0).round() as i64,
                ord_status: if filled {
                    ord_status::FILLED
                } else {
                    ord_status::PARTIALLY_FILLED
                },
                credit_tag: entry.credit_tag.clone(),
                clearing_firm: entry.clearing_firm.clone(),
                branch_id: entry.branch.clone(),
                trd_cnfm_id: stats.next_exec_id(),
                ord_cnfm_id: if entry.order_id.is_empty() {
                    stats.next_order_id()
                } else {
                    entry.order_id.clone()
                },
                trade_date: protocol::now_date(),
                transact_time: protocol::now_ntime(),
                user_info: entry.user_info.clone(),
            };
            let desc = format!(
                "手动成交回报(103) ClOrdID={} 价格={:.5} 数量={} 剩余={}",
                entry.cl_ord_id, fill_px, fill_shares, leaves
            );
            let update = OrderUpdate {
                order_id: entry.order_id.clone(),
                cum_qty: entry.cum_qty + fill_shares as f64,
                leaves_qty: leaves,
                status: if filled {
                    OrderStatus::Filled
                } else {
                    OrderStatus::Partial
                },
            };
            Ok((trade.encode(), desc, update))
        }
        // 拒单：默认回执行报告(32) ExecType='8'；勾选“前台拒单”时改回
        // 申报拒绝消息(204)，不进执行报告流
        ManualReportKind::Reject => {
            if front_reject {
                let rej = OrderReject {
                    biz_id: entry.biz_id,
                    biz_pbu: entry.biz_pbu.clone(),
                    cl_ord_id: entry.cl_ord_id.clone(),
                    security_id: entry.security_id.clone(),
                    ord_rej_reason: reason.unwrap_or(1) as u32,
                    trade_date: protocol::now_date(),
                    transact_time: protocol::now_ntime(),
                    user_info: entry.user_info.clone(),
                };
                let desc = format!(
                    "手动前台拒单(204) ClOrdID={} 原因代码={}",
                    entry.cl_ord_id, rej.ord_rej_reason
                );
                let update = OrderUpdate {
                    order_id: String::new(), // 申报拒绝未分配订单确认编号
                    cum_qty: entry.cum_qty,
                    leaves_qty: 0.0,
                    status: OrderStatus::Rejected,
                };
                Ok((rej.encode(), desc, update))
            } else {
                let mut rpt = base_manual_rpt(cfg, stats, entry);
                rpt.exec_type = exec_type::REJECT;
                rpt.ord_status = ord_status::REJECTED;
                rpt.ord_rej_reason = reason.unwrap_or(1) as u32;
                rpt.leaves_qty = 0;
                let desc = format!(
                    "手动拒单回报(32) ClOrdID={} 原因代码={}",
                    entry.cl_ord_id, rpt.ord_rej_reason
                );
                let update = OrderUpdate {
                    order_id: entry.order_id.clone(),
                    cum_qty: entry.cum_qty,
                    leaves_qty: 0.0,
                    status: OrderStatus::Rejected,
                };
                Ok((rpt.encode(), desc, update))
            }
        }
        ManualReportKind::Cancel => {
            let mut rpt = base_manual_rpt(cfg, stats, entry);
            rpt.exec_type = exec_type::CANCELLED;
            rpt.ord_status = ord_status::CANCELLED;
            // 手动撤单没有“撤单请求编号”，原单号同时填 ClOrdID/OrigClOrdID
            rpt.orig_cl_ord_id = entry.cl_ord_id.clone();
            rpt.leaves_qty = 0;
            rpt.cxl_qty = (entry.leaves_qty * 1000.0).round() as i64;
            // 撤单成功分配新的确认编号，原订单编号回填到 OrigOrdCnfmID
            rpt.ord_cnfm_id = stats.next_order_id();
            rpt.orig_ord_cnfm_id = entry.order_id.clone();
            let desc = format!("手动撤单成功回报(32) ClOrdID={}", entry.cl_ord_id);
            let update = OrderUpdate {
                order_id: entry.order_id.clone(),
                cum_qty: entry.cum_qty,
                leaves_qty: 0.0,
                status: OrderStatus::Cancelled,
            };
            Ok((rpt.encode(), desc, update))
        }
    }
}

/// 手动回报的公共骨架：把订单缓存里登记的信息回填进执行报告(32)，
/// 再补上新的回报序号/确认编号；执行类型/状态由调用方再改
fn base_manual_rpt(
    cfg: &PlatformConfig,
    stats: &PlatformStats,
    entry: &OrderEntry,
) -> ExecRpt {
    // 现货竞价用登录分区号，其余业务用表 3.2.1 固定 SetID
    let biz = strategy::biz_info(entry.biz_id);
    let set_id = if biz.set_id == 0 { cfg.partition_for(&entry.security_id) as u32 } else { biz.set_id };
    ExecRpt {
        pbu: entry.pbu.clone(),
        set_id,
        report_index: 0, // 发送时由 writer 任务补写
        biz_id: entry.biz_id,
        exec_type: exec_type::NEW,
        biz_pbu: entry.biz_pbu.clone(),
        cl_ord_id: entry.cl_ord_id.clone(),
        security_id: entry.security_id.clone(),
        account: entry.account.clone(),
        owner_type: entry.owner_type as u8,
        side: side_byte(&entry.side),
        price: (entry.price * 100000.0).round() as i64,
        order_qty: (entry.qty * 1000.0).round() as i64,
        leaves_qty: (entry.leaves_qty * 1000.0).round() as i64,
        cxl_qty: 0,
        ord_type: entry.ord_type,
        time_in_force: 0,
        ord_status: ord_status::NEW,
        credit_tag: entry.credit_tag.clone(),
        orig_cl_ord_id: String::new(),
        clearing_firm: entry.clearing_firm.clone(),
        branch_id: entry.branch.clone(),
        ord_rej_reason: 0,
        ord_cnfm_id: if entry.order_id.is_empty() {
            stats.next_order_id()
        } else {
            entry.order_id.clone()
        },
        orig_ord_cnfm_id: String::new(),
        trade_date: protocol::now_date(),
        transact_time: protocol::now_ntime(),
        user_info: entry.user_info.clone(),
        // 手动回报无法还原扩展字段，按空值占位
        extend: ExtendFields::default(),
    }
}

/// 把订单缓存里的方向中文转回协议字节（'1'买/'2'卖，手动回报回填用）
fn side_byte(side: &str) -> u8 {
    if side.contains("卖") {
        b'2'
    } else {
        b'1'
    }
}

/// 买卖方向码转中文（仅用于日志展示）。
fn side_name(side: u8) -> &'static str {
    match side {
        b'1' => "买入",
        b'2' => "卖出",
        _ => "未知",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orderbook::{OrderEntry, OrderStatus};
    use crate::shjj::protocol::{BIZ_ID_CASH_AUCTION, BIZ_ID_FUND_TRANSFER, BIZ_ID_RIGHTS};

    /// 模拟缓存里的一笔订单：已收到确认回报（order_id 已回填）
    fn sample_entry() -> OrderEntry {
        OrderEntry {
            cl_ord_id: "A000000001".into(),
            order_id: "0000000000000001".into(),
            security_id: "600000".into(),
            side: "买".into(),
            price: 12.34,
            qty: 100.0,
            cum_qty: 0.0,
            leaves_qty: 100.0,
            status: OrderStatus::New,
            ord_type: b'2',
            account: "B880000001".into(),
            branch: "BR01".into(),
            ts: "09:30:00".into(),
            conn_id: 1,
            // 模拟：委托上报时按登录 CompID 登记了分区 Pbu、回填了 BizID 等
            pbu: "PBU00001".into(),
            biz_id: BIZ_ID_CASH_AUCTION,
            biz_pbu: "PBU00001".into(),
            owner_type: 0,
            credit_tag: String::new(),
            clearing_firm: "CF01".into(),
            user_info: "UINFO".into(),
            biz: String::new(),
        }
    }

    /// 手动拒单（原因码 1025）编码字节验证：报文必须是合法拒单，
    /// 即 MsgType=32 + ExecType='8' + OrdStatus='8' + OrdRejReason=1025，
    /// 绝不能编成撤单成功（ExecType='4'）。
    #[test]
    fn manual_reject_frame_is_reject_not_cancel() {
        let cfg = PlatformConfig::default();
        let stats = PlatformStats::default();
        let (frame, desc, update) = build_manual_report(
            &cfg,
            &stats,
            &sample_entry(),
            ManualReportKind::Reject,
            None,
            None,
            Some(1025),
            false,
        )
        .unwrap();
        // 帧 = 头16 + 消息体213 + 校验和4；MsgType = 32（申报响应/撤单成功执行报告）
        assert_eq!(frame.len(), 16 + 213 + 4);
        assert_eq!(u32::from_be_bytes(frame[0..4].try_into().unwrap()), 32);
        // Pbu 在消息体偏移 0..8（帧内 16..24）：必须回填登录 Pbu，不能是空格
        let pbu = String::from_utf8_lossy(&frame[16..24]).trim().to_string();
        assert_eq!(pbu, "PBU00001", "Pbu 必须回填登录分区（当前={:?}）", pbu);
        // BizID 在消息体偏移 20..24（帧内 36..40）：回填委托携带的业务标识
        assert_eq!(
            u32::from_be_bytes(frame[36..40].try_into().unwrap()),
            BIZ_ID_CASH_AUCTION
        );
        // ExecType 在消息体偏移 24（Pbu8+SetID4+ReportIndex8+BizID4），帧内 16+24=40
        assert_eq!(frame[40], b'8', "ExecType 必须是 '8'（申报拒绝），实际={:#04x}", frame[40]);
        // OrdStatus 在消息体偏移 104（…LeavesQty8+CxlQty8+OrdType1+TimeInForce1 之后），帧内 16+104=120
        assert_eq!(frame[120], b'8', "OrdStatus 必须是 '8'（已拒绝），实际={:#04x}", frame[120]);
        // OrdRejReason 在消息体偏移 133（…OrigClOrdID10+ClearingFirm8+BranchID8 之后），帧内 149..153
        let reason = u32::from_be_bytes(frame[149..153].try_into().unwrap());
        assert_eq!(reason, 1025, "OrdRejReason 必须等于 1025，实际={}", reason);
        // 日志描述与订单状态更新也要对得上
        assert!(desc.contains("手动拒单回报(32)"), "desc={}", desc);
        assert_eq!(update.status, OrderStatus::Rejected);
    }

    /// 手动撤单成功帧对照：同样位置必须是 '4'/'4'，防止两边分支写串
    #[test]
    fn manual_cancel_frame_is_cancel() {
        let cfg = PlatformConfig::default();
        let stats = PlatformStats::default();
        let (frame, desc, _) = build_manual_report(
            &cfg,
            &stats,
            &sample_entry(),
            ManualReportKind::Cancel,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(u32::from_be_bytes(frame[0..4].try_into().unwrap()), 32);
        assert_eq!(frame[40], b'4', "ExecType 必须是 '4'（撤销成功）");
        assert_eq!(frame[120], b'4', "OrdStatus 必须是 '4'（已撤销）");
        assert!(desc.contains("手动撤单成功回报(32)"), "desc={}", desc);
    }

    /// 手动成交回报（MsgType=103）帧头类型正确
    #[test]
    fn manual_trade_frame_is_trade() {
        let cfg = PlatformConfig::default();
        let stats = PlatformStats::default();
        let (frame, desc, _) = build_manual_report(
            &cfg,
            &stats,
            &sample_entry(),
            ManualReportKind::Trade,
            None,
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(u32::from_be_bytes(frame[0..4].try_into().unwrap()), 103);
        assert!(desc.contains("手动成交回报(103)"), "desc={}", desc);
    }

    /// 表 3.2.1 无成交确认的业务不能手动回复成交（参照深市同规则）
    #[test]
    fn manual_trade_rejected_for_no_trade_biz() {
        let cfg = PlatformConfig::default();
        let stats = PlatformStats::default();
        // 转托管 300060：无成交确认，手动成交必须被拒绝
        let mut entry = sample_entry();
        entry.biz_id = BIZ_ID_FUND_TRANSFER;
        let r = build_manual_report(
            &cfg,
            &stats,
            &entry,
            ManualReportKind::Trade,
            None,
            None,
            None,
            false,
        );
        assert!(r.is_err(), "转托管不应允许手动成交");
        assert!(r.unwrap_err().contains("无成交回报"));
        // 配股 300020：有成交确认，允许手动成交（MsgType=103）
        let mut entry2 = sample_entry();
        entry2.biz_id = BIZ_ID_RIGHTS;
        let (frame, desc, _) = build_manual_report(
            &cfg, &stats, &entry2, ManualReportKind::Trade, None, None, None, false,
        )
        .unwrap();
        assert_eq!(u32::from_be_bytes(frame[0..4].try_into().unwrap()), 103);
        assert!(desc.contains("手动成交回报(103)"), "desc={}", desc);
    }

    /// 手动确认回报：执行报告(32) ExecType='0'（申报成功），订单保持已报
    #[test]
    fn manual_ack_frame_is_new() {
        let cfg = PlatformConfig::default();
        let stats = PlatformStats::default();
        let (frame, desc, update) = build_manual_report(
            &cfg, &stats, &sample_entry(), ManualReportKind::Ack, None, None, None, false,
        )
        .unwrap();
        assert_eq!(u32::from_be_bytes(frame[0..4].try_into().unwrap()), 32);
        assert_eq!(frame[40], b'0', "ExecType 必须是 '0'（申报成功）");
        assert!(desc.contains("手动确认回报(32)"), "desc={}", desc);
        // 确认不改变在途状态，订单仍可继续回复成交/拒单/撤单
        assert_eq!(update.status, OrderStatus::New);
        assert_eq!(update.leaves_qty, 100.0);
    }

    /// 前台拒单（front_reject=true）：改发申报拒绝(204)，不进执行报告流
    #[test]
    fn manual_front_reject_sends_order_reject() {
        let cfg = PlatformConfig::default();
        let stats = PlatformStats::default();
        let (frame, desc, update) = build_manual_report(
            &cfg, &stats, &sample_entry(), ManualReportKind::Reject, None, None, Some(1025), true,
        )
        .unwrap();
        assert_eq!(u32::from_be_bytes(frame[0..4].try_into().unwrap()), 204);
        assert!(desc.contains("手动前台拒单(204)"), "desc={}", desc);
        assert_eq!(update.status, OrderStatus::Rejected);
        // 原因代码 1025 落在 OrdRejReason（204 消息体 offset 34..38：BizID4+BizPbu8+ClOrdID10+SecurityID12）
        let reason = u32::from_be_bytes(frame[16 + 34..16 + 38].try_into().unwrap());
        assert_eq!(reason, 1025);
    }
}
