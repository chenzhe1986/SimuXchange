//! 深交所 TCP 会话管理：Logon/Logout/心跳/委托处理
//!
//! # 连接生命周期
//!
//! 柜台（OMS）每连上一个网关监听端口即产生一个会话，完整生命周期如下：
//!
//! ```text
//! 柜台发起 TCP 连接
//!   → 网关等待 Logon 登录报文（可选校验密码）
//!   → 登录成功：回复 Logon 确认 + 下发平台信息/平台状态
//!   → 进入正常工作期：
//!       - 双方按约定间隔互发心跳，证明“我还活着”
//!       - 收到新订单(1xxx01，28 种业务) → 按策略回送确认/成交/拒绝回报
//!       - 收到撤单(190007) → 在途单按原单业务回撤单成功(2xxx02)，否则回撤单失败(290008)
//!   → 结束：柜台注销 / 连接断开 / 心跳超时 / 网关停止
//!   → 清理：从连接表移除，记录日志
//! ```
//!
//! # 心跳超时检测
//!
//! 未单独启动“心跳检查”任务：read_frame 读取报文时携带超时
//! （心跳间隔的 3 倍）。只要对端持续发消息（含心跳），读超时被不断
//! 推迟；长时间收不到任何字节则 read_frame 返回超时错误，会话随之
//! 结束——即常见的空闲检测机制。

use crate::capture::ConnRecorder;
use crate::config::{PlatformConfig, StrategyConfig};
use crate::event::{ConnInfo, EngineEvent};
use crate::orderbook::{OrderBook, OrderEntry, OrderStatus, OrderUpdate};
use super::protocol::{
    self as protocol, exec_type, msg_type, ord_status, CancelReject, ExecRptAck,
    ExecRptTrade, Logon, Logout, NewOrder, OrderCancelRequest,
};
use crate::stats::PlatformStats;
use super::strategy::{self as strategy, biz_info_by_appl_id, ReportKind};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};

/// 全局连接序号发生器：每来一个新连接就 +1，作为连接的唯一编号。
/// AtomicU64 是“原子整数”，多个任务同时加 1 也不会算错，无需加锁。
static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// 会话上下文：会话处理时需要用到的“公共资料袋”。
///
/// 由 engine 在启动平台监听时创建一份，之后每接受一个柜台连接就
/// clone 一份传进去。注意里面的字段全是 Arc（共享引用）或可低成本
/// 克隆的类型，所以 clone 很便宜，且大家操作的仍是同一份统计器、
/// 同一张连接表。
#[derive(Clone)]
pub struct SessionCtx {
    /// 所属网关 id（写日志时标注来源用）
    pub gateway_id: String,
    /// 所属网关名称（拼进日志文本，方便人看）
    pub gateway_name: String,
    /// 平台配置（端口、密码等静态字段），只读共享
    pub cfg: Arc<PlatformConfig>,
    /// 运行时回报策略（模拟策略/回报延迟可热更新：网关运行中修改
    /// 后对后续委托实时生效；每次生成回报计划时加锁读取最新值）
    pub strategy: Arc<std::sync::RwLock<StrategyConfig>>,
    /// 平台统计计数器（委托数/成交数…），各连接共同累加
    pub stats: Arc<PlatformStats>,
    /// 当前存活连接表：连接建立时登记、断开时移除，供前端展示
    pub connections: Arc<StdMutex<HashMap<u64, ConnInfo>>>,
    /// 各连接的收发报文记录器（仅当平台开启展示或持久化时登记）
    pub recorders: Arc<StdMutex<HashMap<u64, Arc<ConnRecorder>>>>,
    /// 各连接的回报发送通道（手动回复成交/拒单/撤单时，把回报投给对应连接）
    pub conn_tx: Arc<StdMutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>,
    /// 订单缓存（平台开启“缓存订单”时创建；登记委托、撤单判断用）
    pub orders: Option<Arc<OrderBook>>,
    /// 数据目录（报文文件写到 <data_dir>/packets/ 下）
    pub data_dir: Arc<PathBuf>,
    /// 平台级报文全局序号发生器（同一平台所有连接共享，跨连接唯一）
    pub capture_seq: Arc<AtomicU64>,
    /// 日志事件广播通道的发送端（最终推送到前端日志面板）
    pub events: broadcast::Sender<EngineEvent>,
}

impl SessionCtx {
    /// 发一条带“[网关名/平台名]”前缀的日志事件。
    /// send 失败只说明当前没有订阅者（如前端还没打开），忽略即可。
    /// pub(crate)：shjj/shbond 会话模块复用同一份上下文与日志格式。
    pub(crate) fn log(&self, level: &str, msg: String) {
        let _ = self.events.send(EngineEvent::log(
            level,
            &self.gateway_id,
            &self.cfg.id,
            format!("[{}/{}] {}", self.gateway_name, self.cfg.name, msg),
        ));
    }

    /// 为一个新连接创建报文记录器（三套会话共用）。
    /// 仅当平台配置开启“展示收发报文”或“持久化到文件”任一开关时才创建；
    /// 开启持久化时报文追加写入
    /// <data_dir>/packets/<YYYYMMDD>/<网关名>_<平台名>/pkg_<接入时间>_ip-<IP>_port-<端口>.log
    /// （按日期分文件夹、日期下再按“网关名_平台名”分文件夹；文件名带接入
    /// 时间与柜台地址，一眼知道是几点从哪台柜台接入的连接）。
    pub(crate) fn start_capture(
        &self,
        conn_id: u64,
        peer: &SocketAddr,
    ) -> Option<Arc<ConnRecorder>> {
        if !self.cfg.show_packets && !self.cfg.persist_packets {
            return None;
        }
        let path = if self.cfg.persist_packets {
            // 目录：packets/<YYYYMMDD>/<网关名>_<平台名>/（网关名与平台名都清洗，防非法字符）
            let date = chrono::Local::now().format("%Y%m%d");
            Some(
                self.data_dir
                    .join("packets")
                    .join(date.to_string())
                    .join(format!(
                        "{}_{}",
                        sanitize_file_name(&self.gateway_name),
                        sanitize_file_name(&self.cfg.name)
                    ))
                    .join(packet_file_name(peer)),
            )
        } else {
            None
        };
        let rec = Arc::new(ConnRecorder::new(
            conn_id,
            self.capture_seq.clone(),
            self.cfg.persist_packets,
            path.as_deref(),
            peer.to_string(),
            chrono::Local::now().format("%H:%M:%S").to_string(),
        ));
        self.recorders.lock().unwrap().insert(conn_id, rec.clone());
        Some(rec)
    }
}

/// 报文文件名：pkg_<接入时间HHMMSS>_ip-<IP(点转下划线)>_port-<端口>.log。
/// IP 里的点替换成下划线、非法字符清洗，保证任意地址都能作为文件名落盘。
fn packet_file_name(peer: &SocketAddr) -> String {
    let ip = sanitize_file_name(&peer.ip().to_string().replace('.', "_"));
    format!(
        "pkg_{}_ip-{}_port-{}.log",
        chrono::Local::now().format("%H%M%S"),
        ip,
        peer.port()
    )
}

/// 把名字里的非法字符替换成下划线（Windows 文件名不允许 \ / : * ? " < > |），
/// 空名给个默认值——用于报文文件目录/文件名，保证任意平台名/网关名都能落盘。
fn sanitize_file_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() { "未命名".into() } else { trimmed.into() }
}

/// 从 TCP 流中读取一条完整报文，返回（消息类型, 消息体, 校验和是否正确, 完整原始帧字节）。
///
/// 按协议格式分三步读：8 字节头（MsgType+BodyLength）→ 消息体 → 4 字节校验和。
/// TCP 是“字节流”，不保证一次能收到一整条报文，read_exact 会一直等到
/// 凑够指定字节数才返回，这正好解决了“拆包”问题。
///
/// 返回的最后一项是“头+体+校验和”拼成的完整原始帧，供报文捕获原样记录。
///
/// idle_timeout 只加在“等报文头”这一步：如果对方长时间一个字节都不发
/// （连心跳都没有），就判定连接已失联，返回超时错误让会话结束。
async fn read_frame(
    rh: &mut OwnedReadHalf,
    idle_timeout: Duration,
) -> std::io::Result<(u32, Vec<u8>, bool, Vec<u8>)> {
    let mut head = [0u8; 8];
    tokio::time::timeout(idle_timeout, rh.read_exact(&mut head))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "读取超时（心跳丢失）"))??;
    // 报文头是大端序：前 4 字节消息类型，后 4 字节消息体长度
    let mt = u32::from_be_bytes(head[0..4].try_into().unwrap());
    let body_len = u32::from_be_bytes(head[4..8].try_into().unwrap()) as usize;
    // 防御性检查：长度大得离谱（>8MB）说明数据错乱或恶意报文，
    // 直接报错断开，避免按这个长度分配内存把程序撑爆
    if body_len > 8 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("消息体长度异常: {}", body_len),
        ));
    }
    let mut body = vec![0u8; body_len];
    rh.read_exact(&mut body).await?;
    let mut cks_buf = [0u8; 4];
    rh.read_exact(&mut cks_buf).await?;
    let recv_cks = u32::from_be_bytes(cks_buf);
    // 自己对“头+体”重算一遍校验和，与对方发来的比对，验证数据没被损坏
    let mut all = Vec::with_capacity(8 + body_len + 4);
    all.extend_from_slice(&head);
    all.extend_from_slice(&body);
    let ok = protocol::checksum(&all) == recv_cks;
    // 拼上校验和字节构成完整原始帧，供报文捕获原样记录
    all.extend_from_slice(&cks_buf);
    Ok((mt, body, ok, all))
}

/// 处理一个柜台连接的完整生命周期（本文件的“主函数”，见模块头的流程图）。
///
/// 参数说明：
/// - ctx：会话上下文（配置、统计器、日志通道等）
/// - stream：已建立的 TCP 连接
/// - peer：对方的 IP:端口（写日志用）
/// - shutdown：网关停止信号的接收端，收到 true 表示要优雅关闭
pub async fn handle_conn(
    ctx: SessionCtx,
    stream: TcpStream,
    peer: SocketAddr,
    mut shutdown: watch::Receiver<bool>,
) {
    let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    // 关闭 Nagle 算法：小报文立即发出，不为攒包而等待，降低回报延迟
    let _ = stream.set_nodelay(true);
    // 把连接拆成“读半边”和“写半边”，读写可以在不同任务里独立进行
    let (mut rh, mut wh) = stream.into_split();

    // 报文捕获：平台开启展示/持久化时为本连接建一个记录器（否则为 None，零开销）
    let recorder = ctx.start_capture(conn_id, &peer);
    let rec_send = recorder.clone();

    // 写通道：可能有多方要给柜台发消息（登录回复、心跳任务、延迟回报任务…），
    // 若各自直接写 socket 会互相穿插打乱字节流。因此统一投递到这个 mpsc
    // 队列，由下面唯一的 writer 任务按先来后到逐条写出，天然保证顺序。
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4096);
    // 登记发送通道：界面手动回复成交/拒单/撤单时，经此把回报投给本连接
    ctx.conn_tx.lock().unwrap().insert(conn_id, tx.clone());
    let writer = tokio::spawn(async move {
        // 队列里有消息就写出去；写失败（对方已断开）或队列关闭则退出
        while let Some(buf) = rx.recv().await {
            // 发送前捕获（深交所报文写出前不再加工，字节即上线内容），
            // 并同步解析字段：头 8 字节消息类型，消息体在 [8..len-4]
            if let Some(r) = &rec_send {
                let fields = if buf.len() >= 12 {
                    let mt = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                    protocol::describe_fields(mt, &buf[8..buf.len() - 4])
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

    // 登记连接：计入累计连接数，并加入存活连接表（前端可以看到）
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
    ctx.log("info", format!("柜台 {} 已连接，等待 Logon", peer));

    let mut hb_secs: u64 = 30; // 心跳间隔（秒），Logon 时以对方要求为准
    let mut logged_on = false; // 是否已登录（未登录前拒绝处理业务消息）
    let mut hb_task: Option<tokio::task::JoinHandle<()>> = None; // 心跳发送任务句柄

    // 主循环：一次处理一条报文，直到连接结束
    loop {
        // 空闲超时 = 心跳间隔 × 3（宽容两次丢失），且不低于 10 秒
        let idle = Duration::from_secs((hb_secs * 3).max(10));
        // select! 同时等两件事：网关停止信号 / 下一条报文，谁先来处理谁
        let frame = tokio::select! {
            _ = shutdown.changed() => {
                // 网关停止：主动注销
                let _ = tx.send(Logout { session_status: 4, text: "网关停止".into() }.encode()).await;
                break;
            }
            r = read_frame(&mut rh, idle) => match r {
                Ok(f) => f,
                Err(e) => {
                    ctx.log("warn", format!("柜台 {} 连接断开: {}", peer, e));
                    break;
                }
            }
        };
        let (mt, body, cks_ok, raw) = frame;
        // 捕获收到的报文（校验和错误的也记录，便于排查），同时按字段解析
        if let Some(r) = &recorder {
            r.record_recv(&raw, protocol::describe_fields(mt, &body));
        }
        // 校验和不对：数据可能在传输中损坏，丢弃这一条，继续收下一条
        if !cks_ok {
            ctx.log("warn", format!("柜台 {} 消息校验和错误 MsgType={}，已忽略", peer, mt));
            continue;
        }

        // 按消息类型分发处理
        match mt {
            msg_type::LOGON => {
                let logon = match Logon::decode(&body) {
                    Ok(l) => l,
                    Err(e) => {
                        ctx.log("error", format!("Logon 消息解析失败: {}", e));
                        break;
                    }
                };
                // 若平台配置开启了密码校验，密码不符则回 Logout(状态5) 并断开
                if ctx.cfg.check_password && logon.password != ctx.cfg.password {
                    ctx.log("warn", format!("柜台 {} 登录密码错误，拒绝登录", peer));
                    let _ = tx
                        .send(Logout { session_status: 5, text: "不合法的用户名或口令".into() }.encode())
                        .await;
                    break;
                }
                // 心跳间隔采用对方 Logon 里的要求，限制在 1~300 秒的合理范围
                hb_secs = logon.heart_bt_int.clamp(1, 300) as u64;
                // 回复 Logon 确认登录（真实交易所的握手流程也是如此）
                let reply = Logon {
                    sender_comp_id: ctx.cfg.comp_id.clone(),
                    target_comp_id: logon.sender_comp_id.clone(),
                    heart_bt_int: logon.heart_bt_int,
                    password: String::new(),
                    default_appl_ver_id: if logon.default_appl_ver_id.is_empty() {
                        "1.29".into()
                    } else {
                        logon.default_appl_ver_id.clone()
                    },
                };
                let _ = tx.send(reply.encode()).await;
                // 登录成功后下发平台信息、平台状态（状态 2 = 开放，可以报单）
                let _ = tx
                    .send(protocol::encode_platform_info(
                        ctx.cfg.platform_type,
                        &[ctx.cfg.partition_no],
                    ))
                    .await;
                let _ = tx
                    .send(protocol::encode_platform_state(ctx.cfg.platform_type, 2))
                    .await;
                logged_on = true;
                if let Some(c) = ctx.connections.lock().unwrap().get_mut(&conn_id) {
                    c.comp_id = logon.sender_comp_id.clone();
                    c.logged_on = true;
                }
                ctx.log(
                    "info",
                    format!(
                        "柜台 {} 登录成功 CompID={} 心跳间隔={}s",
                        peer, logon.sender_comp_id, hb_secs
                    ),
                );
                // 启动心跳定时任务：每隔 hb_secs 秒给柜台发一条心跳。
                // 若柜台重复 Logon，先停掉旧任务，避免出现两个心跳源
                if let Some(h) = hb_task.take() {
                    h.abort();
                }
                let hb_tx = tx.clone();
                hb_task = Some(tokio::spawn(async move {
                    let mut iv = tokio::time::interval(Duration::from_secs(hb_secs));
                    iv.tick().await; // 跳过立即触发的第一次
                    loop {
                        iv.tick().await;
                        if hb_tx.send(protocol::encode_heartbeat()).await.is_err() {
                            break;
                        }
                    }
                }));
            }
            msg_type::HEARTBEAT => {
                // 收到心跳不需要任何回应——它的作用已经起到了：
                // read_frame 每收到一条报文就重新开始计时，空闲超时被刷新
            }
            msg_type::LOGOUT => {
                // 柜台主动注销：回一条确认后结束会话
                let status = Logout::decode(&body).map(|l| l.session_status).unwrap_or(0);
                ctx.log("info", format!("柜台 {} 请求注销（状态={}）", peer, status));
                let _ = tx
                    .send(Logout { session_status: 4, text: "会话退登完成".into() }.encode())
                    .await;
                break;
            }
            msg_type::REPORT_SYNC => {
                // 回报同步请求：模拟器不保存历史回报，记条日志即可
                ctx.log("info", format!("柜台 {} 发送回报同步请求", peer));
            }
            // “if logged_on”是匹配守卫：只有登录后才接受委托/撤单，
            // 未登录时会落入下面的 other 分支被忽略
            // 4.5.1 新订单：28 种业务消息类型统一分发（m 绑定实际消息类型，
            // handle_new_order 按业务解码委托并选回报报文类型）
            m if logged_on && NewOrder::is_new_order(m) => {
                handle_new_order(m, &ctx, &tx, &body, conn_id).await;
            }
            msg_type::ORDER_CANCEL_REQUEST if logged_on => {
                handle_cancel(&ctx, &tx, &body).await;
            }
            other => {
                if !logged_on {
                    ctx.log("warn", format!("柜台 {} 未登录即发送消息 MsgType={}，忽略", peer, other));
                } else {
                    ctx.log("warn", format!("收到暂不支持的消息 MsgType={}，忽略", other));
                }
            }
        }
    }

    // ── 会话结束，开始清理 ──
    if let Some(h) = hb_task {
        h.abort(); // 停止心跳任务
    }
    // 先移除发送通道：它持有一份 tx clone，不先移除的话 writer 的 recv
    // 永远等不到队列关闭，writer.await 会一直挂在这里，后续的
    // connections 移除 / mark_dead 都执行不到（重连会被单连接限制拒绝）
    ctx.conn_tx.lock().unwrap().remove(&conn_id);
    drop(tx); // 关闭写通道发送端，writer 任务的 recv 返回 None 随之退出
    let _ = writer.await; // 等 writer 把队列里剩余消息（如 Logout）发完再走
    ctx.connections.lock().unwrap().remove(&conn_id); // 从存活连接表移除
    // 记录器保留不删：平台级报文弹窗要能按连接分组回看本连接的历史报文，
    // 这里只标记为“已离线”（分组标题上显示）
    if let Some(r) = &recorder {
        r.mark_dead();
    }
    ctx.log("info", format!("柜台 {} 会话结束", peer));
}

/// 处理新订单（4.5.1，MsgType=1xxx01，共 28 种业务消息类型）：核心业务入口。
///
/// 流程：按消息类型解析委托 → 计入统计 → 让 strategy 模块按业务特征表
/// （BizInfo）生成“回报计划”（每条含：延迟毫秒数 + 编码好的报文 + 类型 + 描述）
/// → 按计划逐条发送给柜台。
async fn handle_new_order(
    mt: u32,
    ctx: &SessionCtx,
    tx: &mpsc::Sender<Vec<u8>>,
    body: &[u8],
    conn_id: u64,
) {
    let order = match NewOrder::decode(mt, body) {
        Ok(o) => o,
        Err(e) => {
            ctx.log("error", format!("新订单({})解析失败: {}", mt, e));
            return;
        }
    };
    ctx.stats.orders.fetch_add(1, Ordering::Relaxed);
    // 日志里把协议的放大整数还原成人类可读的值：价格÷10000，数量÷100
    ctx.log(
        "info",
        format!(
            "收到委托 MsgType={} ApplID={} ClOrdID={} 证券={} 方向={} 价格={:.4} 数量={}",
            mt,
            order.common.appl_id,
            order.common.cl_ord_id,
            order.common.security_id,
            side_name(order.common.side),
            order.common.price as f64 / 10000.0,
            order.common.order_qty / 100
        ),
    );

    // 缓存订单：登记一条新订单（状态=已报），回报逐条发出时再更新状态，
    // 这样撤单请求到达时能查到订单的真实当前状态；conn_id 记录订单来自
    // 哪个连接（手动回复成交/拒单/撤单时按它定位发送通道）
    if let Some(ob) = &ctx.orders {
        ob.add(OrderEntry {
            cl_ord_id: order.common.cl_ord_id.clone(),
            order_id: String::new(),
            security_id: order.common.security_id.clone(),
            side: side_name(order.common.side).to_string(),
            price: order.common.price as f64 / 10000.0,
            qty: order.common.order_qty as f64 / 100.0,
            cum_qty: 0.0,
            leaves_qty: order.common.order_qty as f64 / 100.0,
            status: OrderStatus::New,
            ord_type: order.common.ord_type,
            account: order.common.account_id.clone(),
            branch: order.common.branch_id.clone(),
            ts: chrono::Local::now().format("%H:%M:%S").to_string(),
            conn_id,
            // 协议回填字段：手动回复回报时与自动回报保持一致
            // （深市无 Pbu 分区机制，pbu 存申报交易单元，回报两字段共用）
            pbu: order.common.submitting_pbu_id.clone(),
            biz_id: 0,
            biz_pbu: String::new(),
            owner_type: order.common.owner_type,
            credit_tag: String::new(),
            clearing_firm: order.common.clearing_firm.clone(),
            user_info: order.common.user_info.clone(),
            // 业务标识 = 委托的 ApplID：撤单成功/手动回复按它反查业务特征
            biz: order.common.appl_id.clone(),
        });
    }

    // 生成回报计划（具体几条确认/成交、延迟多少，由运行时策略配置决定；
    // 策略支持热更新，每次委托都读取最新值，改动即时生效）
    // 注意：策略读锁只在生成计划期间持有，不跨越 await 点——
    // RwLockReadGuard 不是 Send，跨 await 存活会阻止会话任务在线程间调度
    let plans = {
        let st = ctx.strategy.read().unwrap();
        strategy::plan_reports(&st, ctx.cfg.partition_no, &order, &ctx.stats)
    };
    // 所有回报延迟都为 0 时可以当场发完；否则需要另起任务慢慢发
    let all_sync = plans.iter().all(|p| p.delay_ms == 0);
    let ctx2 = ctx.clone();
    let tx2 = tx.clone();
    // 把“逐条发送”的逻辑包成一个异步代码块，下面再决定怎么跑它
    let fut = async move {
        for p in plans {
            if p.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(p.delay_ms)).await;
            }
            if tx2.send(p.frame).await.is_err() {
                break; // 连接已断开，剩余回报不必再发
            }
            // 按回报类型累加对应的统计计数（前端统计面板的数据来源）
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
            // （计划里带着委托编号，对号入座）
            if let Some(upd) = &p.order_update {
                if let Some(ob) = &ctx2.orders {
                    ob.apply(&p.cl_ord_id, upd);
                }
            }
            ctx2.log("info", format!("发送 {}", p.desc));
        }
    };
    if all_sync {
        // 延迟全为 0：在当前消息处理流程内同步发送，
        // 保证回报一定排在下一笔委托的回报之前（严格保序）
        fut.await;
    } else {
        // 存在延迟：交给独立任务去睡眠+发送，主循环立即继续
        // 接收下一笔委托，不会被延迟卡住
        tokio::spawn(fut);
    }
}

/// 处理撤单请求(190007)。
///
/// 平台开启“缓存订单”时按订单真实状态回复：
/// - 原单在途（已报/部分成交）→ 撤单成功：回执行报告(200102) ExecType=4 已撤，
///   并把缓存里的订单状态置为已撤
/// - 原单已是终态（全成/已拒/已撤）或找不到 → 撤单失败：回撤单拒绝(290008)
/// 未开启缓存时维持旧行为（一律撤单失败，无订单信息可查）。
async fn handle_cancel(ctx: &SessionCtx, tx: &mpsc::Sender<Vec<u8>>, body: &[u8]) {
    let req = match OrderCancelRequest::decode(body) {
        Ok(r) => r,
        Err(e) => {
            ctx.log("error", format!("撤单请求(190007)解析失败: {}", e));
            return;
        }
    };
    ctx.stats.cancels.fetch_add(1, Ordering::Relaxed);

    // ---- 有订单缓存：先尝试按原单状态撤单 ----
    if let Some(ob) = &ctx.orders {
        // cancel_inflight 只在原单存在且为在途时返回 true（并已置为已撤）
        if ob.cancel_inflight(&req.orig_cl_ord_id) {
            let entry = ob.find(&req.orig_cl_ord_id).expect("刚撤单成功的订单必然存在");
            // 撤单成功回报：按原单业务（订单缓存里存的 ApplID）反查确认执行
            // 报告报文类型（表 4-30：各业务撤单成功回各自的 2xxx02），
            // ExecType=4 / OrdStatus=4（已撤）；查不到业务时按现货竞价兜底
            let biz = biz_info_by_appl_id(&entry.biz)
                .unwrap_or_else(|| strategy::biz_info(msg_type::NEW_ORDER_CASH));
            let cxl = ExecRptAck {
                msg_type: biz.ack_msg_type,
                partition_no: ctx.cfg.partition_no,
                report_index: ctx.stats.next_report_index(),
                appl_id: if req.appl_id.is_empty() {
                    biz.appl_id.into()
                } else {
                    req.appl_id.clone()
                },
                reporting_pbu_id: req.submitting_pbu_id.clone(),
                submitting_pbu_id: req.submitting_pbu_id.clone(),
                security_id: req.security_id.clone(),
                security_id_source: if req.security_id_source.is_empty() {
                    "102".into()
                } else {
                    req.security_id_source.clone()
                },
                owner_type: req.owner_type,
                clearing_firm: req.clearing_firm.clone(),
                transact_time: protocol::now_timestamp(),
                user_info: req.user_info.clone(),
                // 订单号/价格/数量回填原单的值（缓存里存的自然单位再放大回协议整数）
                order_id: entry.order_id.clone(),
                cl_ord_id: req.cl_ord_id.clone(),
                orig_cl_ord_id: req.orig_cl_ord_id.clone(),
                exec_id: ctx.stats.next_exec_id(),
                exec_type: exec_type::CANCELLED,
                ord_status: ord_status::CANCELLED,
                ord_rej_reason: 0,
                leaves_qty: 0,
                cum_qty: (entry.cum_qty * 100.0) as i64,
                side: req.side,
                ord_type: entry.ord_type,
                order_qty: (entry.qty * 100.0) as i64,
                price: (entry.price * 10000.0) as i64,
                account_id: entry.account.clone(),
                branch_id: entry.branch.clone(),
                order_restrictions: String::new(),
                // 撤单成功回报无业务扩展字段（表 4-30 扩展仅订单响应使用）
                extend: Default::default(),
            };
            let _ = tx.send(cxl.encode()).await;
            ctx.log(
                "info",
                format!(
                    "收到撤单请求 ClOrdID={} OrigClOrdID={}，原单在途，已回撤单成功({})",
                    req.cl_ord_id, req.orig_cl_ord_id, biz.ack_msg_type
                ),
            );
            return;
        }
        // 原单存在但已是终态：不在这里记录“撤单失败”（日志统一走下面）
    }

    // ---- 撤单失败：未开启缓存 / 找不到原单 / 原单已是终态 ----
    // 拼撤单拒绝报文：大部分字段直接回填请求里的值（回报要能对得上号）
    let rej = CancelReject {
        partition_no: ctx.cfg.partition_no,
        report_index: ctx.stats.next_report_index(),
        appl_id: if req.appl_id.is_empty() { "010".into() } else { req.appl_id.clone() },
        reporting_pbu_id: req.submitting_pbu_id.clone(),
        submitting_pbu_id: req.submitting_pbu_id.clone(),
        security_id: req.security_id.clone(),
        security_id_source: req.security_id_source.clone(),
        owner_type: req.owner_type,
        clearing_firm: req.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: req.user_info.clone(),
        cl_ord_id: req.cl_ord_id.clone(),
        orig_cl_ord_id: req.orig_cl_ord_id.clone(),
        side: req.side,
        ord_status: b'8', // '8' = 已拒绝（未找到原始订单或原单不可撤）
        cxl_rej_reason: 1,
        reject_text: "SIM NO ORDER".into(),
        order_id: req.order_id.clone(),
    };
    let _ = tx.send(rej.encode()).await;
    ctx.log(
        "info",
        format!(
            "收到撤单请求 ClOrdID={} OrigClOrdID={}，原单不存在或不可撤，已回撤单失败响应(290008)",
            req.cl_ord_id, req.orig_cl_ord_id
        ),
    );
}

/// 手动回复的回报种类（界面在途单上选择“成交/拒单/撤单成功”时指定）。
/// 字段名走 snake_case，与前端 send_report 命令的 kind 参数对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualReportKind {
    /// 成交回报（可指定成交数量与价格）
    Trade,
    /// 拒单回报（执行报告形式，可指定拒单原因代码）
    Reject,
    /// 撤单成功回报
    Cancel,
}

/// 构造一笔手动回复报文（成交/拒单/撤单成功）。
///
/// 返回 Ok((编码好的完整帧, 日志描述, 订单缓存同步更新))；
/// 业务不支持成交回报时返回 Err（表 3-4：ETF 申赎/认购/行权等无成交回报）。
/// qty/price 用自然单位（股/元），内部换算成协议放大整数；不传时用
/// 订单缓存里的值兜底：
/// - 成交数量默认剩余量（即全部成交），超过剩余量也按剩余量算
/// - 成交价格默认委托价
/// - 拒单原因默认 1
///
/// 订单缓存里只登记了通用字段（编号/证券/方向/价格数量/账户/营业部），
/// 协议特有的交易单元等字段用空串/默认值占位——柜台按 ClOrdID/数量/价格
/// 关联订单即可，不影响手动回复的业务语义。
pub fn build_manual_report(
    cfg: &PlatformConfig,
    stats: &PlatformStats,
    entry: &OrderEntry,
    kind: ManualReportKind,
    qty: Option<f64>,
    price: Option<f64>,
    reason: Option<i32>,
) -> Result<(Vec<u8>, String, OrderUpdate), String> {
    // 按订单缓存的业务标识（深市 ApplID）反查业务特征，决定回报报文类型；
    // 查不到（老缓存/沪市订单）时按现货竞价兜底
    let biz = biz_info_by_appl_id(&entry.biz)
        .unwrap_or_else(|| strategy::biz_info(msg_type::NEW_ORDER_CASH));
    match kind {
        ManualReportKind::Trade => {
            // 无成交回报的业务（表 3-4）拒绝手动成交：模拟器不编造交易所
            // 不会发的报文
            let Some(trade_msg_type) = biz.trade_msg_type else {
                return Err(format!("业务[{}]无成交回报（表3-4），不能手动回复成交", biz.name));
            };
            // 成交数量钳制到 (0, 剩余量]：不传或超限都按剩余量全成
            let fill_shares = qty
                .map(|q| q as i64)
                .unwrap_or(entry.leaves_qty as i64)
                .clamp(1, entry.leaves_qty as i64);
            let fill_px = price.unwrap_or(entry.price).max(0.0001);
            let leaves = (entry.leaves_qty - fill_shares as f64).max(0.0);
            let filled = leaves == 0.0;
            let trade = ExecRptTrade {
                msg_type: trade_msg_type,
                partition_no: cfg.partition_no,
                report_index: stats.next_report_index(),
                appl_id: biz.appl_id.into(),
                reporting_pbu_id: entry.pbu.clone(),
                submitting_pbu_id: entry.pbu.clone(),
                security_id: entry.security_id.clone(),
                security_id_source: "102".into(),
                owner_type: entry.owner_type,
                clearing_firm: entry.clearing_firm.clone(),
                transact_time: protocol::now_timestamp(),
                user_info: entry.user_info.clone(),
                order_id: entry.order_id.clone(),
                cl_ord_id: entry.cl_ord_id.clone(),
                exec_id: stats.next_exec_id(),
                exec_type: exec_type::TRADE,
                ord_status: if filled {
                    ord_status::FILLED
                } else {
                    ord_status::PARTIALLY_FILLED
                },
                last_px: (fill_px * 10000.0).round() as i64,
                last_qty: fill_shares * 100,
                leaves_qty: (leaves * 100.0).round() as i64,
                cum_qty: ((entry.cum_qty + fill_shares as f64) * 100.0).round() as i64,
                side: side_byte(&entry.side),
                account_id: entry.account.clone(),
                branch_id: entry.branch.clone(),
                extend: Default::default(),
            };
            let desc = format!(
                "手动成交回报({}) ClOrdID={} 价格={:.4} 数量={} 剩余={}",
                trade_msg_type, entry.cl_ord_id, fill_px, fill_shares, leaves
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
        // 拒单/撤单成功共用确认报文骨架（2xxx02），仅执行类型/状态/原因不同
        ManualReportKind::Reject | ManualReportKind::Cancel => {
            let mut rpt = base_manual_ack(cfg, stats, entry, biz);
            match kind {
                ManualReportKind::Reject => {
                    rpt.exec_type = exec_type::REJECT;
                    rpt.ord_status = ord_status::REJECTED;
                    rpt.ord_rej_reason = reason.unwrap_or(1) as u16;
                    rpt.leaves_qty = 0;
                    rpt.cum_qty = (entry.cum_qty * 100.0).round() as i64;
                    let desc = format!(
                        "手动拒单回报({}) ClOrdID={} 原因代码={}",
                        biz.ack_msg_type, entry.cl_ord_id, rpt.ord_rej_reason
                    );
                    let update = OrderUpdate {
                        order_id: entry.order_id.clone(),
                        cum_qty: entry.cum_qty,
                        leaves_qty: 0.0,
                        status: OrderStatus::Rejected,
                    };
                    Ok((rpt.encode(), desc, update))
                }
                ManualReportKind::Cancel => {
                    rpt.exec_type = exec_type::CANCELLED;
                    rpt.ord_status = ord_status::CANCELLED;
                    // 手动撤单没有“撤单请求编号”，原单号同时填 ClOrdID/OrigClOrdID，
                    // 柜台按任一字段都能关联上
                    rpt.orig_cl_ord_id = entry.cl_ord_id.clone();
                    rpt.leaves_qty = 0;
                    rpt.cum_qty = (entry.cum_qty * 100.0).round() as i64;
                    let desc = format!(
                        "手动撤单成功回报({}) ClOrdID={}",
                        biz.ack_msg_type, entry.cl_ord_id
                    );
                    let update = OrderUpdate {
                        order_id: entry.order_id.clone(),
                        cum_qty: entry.cum_qty,
                        leaves_qty: 0.0,
                        status: OrderStatus::Cancelled,
                    };
                    Ok((rpt.encode(), desc, update))
                }
                ManualReportKind::Trade => unreachable!(),
            }
        }
    }
}

/// 手动回报的公共骨架：把订单缓存里登记的信息回填进确认执行报告（2xxx02），
/// 再补上新的回报序号/执行编号；报文类型按业务特征（biz）选择；
/// 执行类型/状态由调用方再改
fn base_manual_ack(
    cfg: &PlatformConfig,
    stats: &PlatformStats,
    entry: &OrderEntry,
    biz: strategy::BizInfo,
) -> ExecRptAck {
    ExecRptAck {
        msg_type: biz.ack_msg_type,
        partition_no: cfg.partition_no,
        report_index: stats.next_report_index(),
        appl_id: biz.appl_id.into(),
        reporting_pbu_id: entry.pbu.clone(),
        submitting_pbu_id: entry.pbu.clone(),
        security_id: entry.security_id.clone(),
        security_id_source: "102".into(),
        owner_type: entry.owner_type,
        clearing_firm: entry.clearing_firm.clone(),
        transact_time: protocol::now_timestamp(),
        user_info: entry.user_info.clone(),
        order_id: entry.order_id.clone(),
        cl_ord_id: entry.cl_ord_id.clone(),
        orig_cl_ord_id: String::new(),
        exec_id: stats.next_exec_id(),
        exec_type: exec_type::NEW,
        ord_status: ord_status::NEW,
        ord_rej_reason: 0,
        leaves_qty: (entry.leaves_qty * 100.0).round() as i64,
        cum_qty: (entry.cum_qty * 100.0).round() as i64,
        side: side_byte(&entry.side),
        ord_type: entry.ord_type,
        order_qty: (entry.qty * 100.0).round() as i64,
        price: (entry.price * 10000.0).round() as i64,
        account_id: entry.account.clone(),
        branch_id: entry.branch.clone(),
        order_restrictions: String::new(),
        extend: Default::default(),
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

/// 把协议里的买卖方向码（'1'/'2'）转成中文，仅供日志显示
fn side_name(side: u8) -> &'static str {
    match side {
        b'1' => "买",
        b'2' => "卖",
        _ => "?",
    }
}
